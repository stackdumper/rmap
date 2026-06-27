//! Crate source resolution: map a crate spec (`name` or `name@version`) to its
//! unpacked source directory in the local cargo registry.
//!
//! Offline only — the crate must already be vendored by a prior `cargo fetch`
//! / `cargo build`. No network access, no `cargo` subprocess. Registry layout:
//!
//! ```text
//! $CARGO_HOME/registry/src/<index-host-hash>/<name>-<version>/
//! ```
//!
//! Multiple index hosts and multiple versions can coexist; we scan them all.

use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};

/// Resolve a crate spec to its unpacked source root in the cargo registry.
///
/// Spec forms:
/// - `name` — version from the nearest `Cargo.lock`, else the highest version
///   present in the registry (with a stderr warning).
/// - `name@version` — exact `<name>-<version>` dir.
///
/// Returns a printable error (with a hint) if nothing matches.
pub fn resolve(spec: &str) -> Result<PathBuf, String> {
    let (name, want_ver) = match spec.split_once('@') {
        Some((n, v)) => (n, Some(v)),
        None => (spec, None),
    };
    if name.is_empty() {
        return Err(format!("error: empty crate name in `{spec}`"));
    }

    let registries = registry_src_dirs();
    if registries.is_empty() {
        return Err(format!(
            "error: no cargo registry source dir found under {}. \
             Run `cargo fetch` first, or set CARGO_HOME.",
            cargo_home().join("registry").join("src").display()
        ));
    }

    // Gather every <name>-<version> dir across all registry index hosts.
    let mut versions: Vec<(String, PathBuf)> = Vec::new();
    for reg in &registries {
        versions.extend(versions_of(reg, name));
    }

    // A bare name defers to the nearest Cargo.lock pin (IO); an explicit
    // `name@version` ignores the lock entirely.
    let lock_ver = match want_ver {
        Some(_) => None,
        None => lock_version(name),
    };

    let chosen = select(name, &versions, want_ver, lock_ver.as_deref())?;
    if let Some(w) = chosen.warn {
        eprintln!("{w}");
    }
    Ok(chosen.path)
}

/// The outcome of version selection: the chosen source dir plus an optional
/// stderr warning the caller should surface.
#[derive(Debug)]
struct Selection {
    path: PathBuf,
    warn: Option<String>,
}

/// Pure version-selection policy, factored out of [`resolve`] so it can be
/// tested without touching the filesystem. `versions` are the `<name>-<ver>`
/// dirs found in the registry; `want_ver` is an explicit `@version` pin;
/// `lock_ver` is the version pinned by Cargo.lock (only for a bare name).
fn select(
    name: &str,
    versions: &[(String, PathBuf)],
    want_ver: Option<&str>,
    lock_ver: Option<&str>,
) -> Result<Selection, String> {
    if versions.is_empty() {
        return Err(format!(
            "error: crate `{name}` not found in the cargo registry. \
             Run `cargo fetch` in a project that depends on it first."
        ));
    }

    let find = |want: &str| versions.iter().find(|(ver, _)| ver == want).map(|(_, p)| p);

    // Explicit `name@version`: exact match or list what's available.
    if let Some(v) = want_ver {
        return find(v)
            .map(|p| Selection {
                path: p.clone(),
                warn: None,
            })
            .ok_or_else(|| {
                let mut avail: Vec<&str> = versions.iter().map(|(v, _)| v.as_str()).collect();
                avail.sort_by(|a, b| cmp_semver(b, a));
                format!(
                    "error: `{name}@{v}` not vendored. Available: {}",
                    avail.join(", ")
                )
            });
    }

    // Bare name: prefer the version pinned by the nearest Cargo.lock.
    if let Some(v) = lock_ver {
        return find(v)
            .map(|p| Selection {
                path: p.clone(),
                warn: None,
            })
            .ok_or_else(|| {
                format!(
                    "error: Cargo.lock pins `{name} {v}` but that version is not \
                     vendored. Run `cargo fetch`."
                )
            });
    }

    // No pin: take the highest version, warn if that was an actual choice.
    let (ver, path) = versions
        .iter()
        .max_by(|(a, _), (b, _)| cmp_semver(a, b))
        .unwrap();
    let warn = (versions.len() > 1).then(|| {
        format!(
            "warning: no Cargo.lock entry for `{name}`; picked highest of {} \
             vendored versions ({ver})",
            versions.len()
        )
    });
    Ok(Selection {
        path: path.clone(),
        warn,
    })
}

/// All `<name>-<version>` source dirs for `name` directly under one registry
/// index-host dir. Filters out same-prefix crates (`serde` vs `serde_json`)
/// by requiring the post-prefix remainder to start with a digit (a version).
fn versions_of(reg: &Path, name: &str) -> Vec<(String, PathBuf)> {
    let prefix = format!("{name}-");
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(reg) else {
        return out;
    };
    for entry in rd.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let fname = entry.file_name();
        let Some(fname) = fname.to_str() else {
            continue;
        };
        let Some(ver) = fname.strip_prefix(&prefix) else {
            continue;
        };
        if !ver.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        out.push((ver.to_string(), entry.path()));
    }
    out
}

/// Walk up from the current dir looking for a `Cargo.lock`; return the version
/// pinned for `name`, if found. Stops at the first lock encountered.
fn lock_version(name: &str) -> Option<String> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let lock = dir.join("Cargo.lock");
        if lock.is_file() {
            return fs::read_to_string(&lock)
                .ok()
                .and_then(|t| parse_lock_version(&t, name));
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Scrape `version` for the `[[package]]` whose `name` matches. Cargo.lock is
/// TOML, but the relevant slice is a trivial, stable line grammar — no need for
/// a full TOML parser.
fn parse_lock_version(text: &str, name: &str) -> Option<String> {
    let mut cur_name: Option<&str> = None;
    for line in text.lines() {
        let line = line.trim();
        if line == "[[package]]" {
            cur_name = None;
        } else if let Some(rest) = line.strip_prefix("name = ") {
            cur_name = Some(rest.trim().trim_matches('"'));
        } else if let Some(rest) = line.strip_prefix("version = ") {
            if cur_name == Some(name) {
                return Some(rest.trim().trim_matches('"').to_string());
            }
        }
    }
    None
}

/// Compare two version strings by numeric (major, minor, patch). Pre-release
/// and build metadata are ignored — adequate for "pick the highest vendored".
fn cmp_semver(a: &str, b: &str) -> Ordering {
    parse_ver(a).cmp(&parse_ver(b))
}

fn parse_ver(v: &str) -> (u64, u64, u64) {
    let core = v.split(['-', '+']).next().unwrap_or(v);
    let mut it = core.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    )
}

fn cargo_home() -> PathBuf {
    if let Some(h) = std::env::var_os("CARGO_HOME") {
        return PathBuf::from(h);
    }
    if let Some(h) = std::env::var_os("HOME") {
        return PathBuf::from(h).join(".cargo");
    }
    PathBuf::from(".cargo")
}

/// The per-index-host dirs under `$CARGO_HOME/registry/src/`.
fn registry_src_dirs() -> Vec<PathBuf> {
    let base = cargo_home().join("registry").join("src");
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(&base) {
        for entry in rd.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                out.push(entry.path());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lock_version_finds_matching_package() {
        let lock = r#"
[[package]]
name = "serde"
version = "1.0.210"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "tokio"
version = "1.40.0"
"#;
        assert_eq!(parse_lock_version(lock, "serde").as_deref(), Some("1.0.210"));
        assert_eq!(parse_lock_version(lock, "tokio").as_deref(), Some("1.40.0"));
        assert_eq!(parse_lock_version(lock, "absent"), None);
    }

    #[test]
    fn parse_lock_version_ignores_name_in_other_fields() {
        // A `name = "serde"` in dependencies list shouldn't be confused with a
        // package header. Our grammar only reads top-level `name =`/`version =`
        // lines per block, so a dependency-table entry like `"serde 1.0"` is
        // never matched as a package name.
        let lock = r#"
[[package]]
name = "serde_json"
version = "1.0.128"
dependencies = [
 "serde",
]
"#;
        assert_eq!(parse_lock_version(lock, "serde"), None);
        assert_eq!(
            parse_lock_version(lock, "serde_json").as_deref(),
            Some("1.0.128")
        );
    }

    #[test]
    fn semver_ordering_numeric_not_lexical() {
        assert_eq!(cmp_semver("1.0.10", "1.0.9"), Ordering::Greater);
        assert_eq!(cmp_semver("0.34.3", "0.4.0"), Ordering::Greater);
        assert_eq!(cmp_semver("2.0.0", "10.0.0"), Ordering::Less);
        assert_eq!(cmp_semver("1.0.0", "1.0.0"), Ordering::Equal);
    }

    #[test]
    fn parse_ver_strips_prerelease_and_build() {
        assert_eq!(parse_ver("1.2.3-rc.1"), (1, 2, 3));
        assert_eq!(parse_ver("0.38.0+1.3.281"), (0, 38, 0));
        assert_eq!(parse_ver("1.0"), (1, 0, 0));
    }

    fn vers(specs: &[&str]) -> Vec<(String, PathBuf)> {
        specs
            .iter()
            .map(|v| (v.to_string(), PathBuf::from(format!("/reg/crate-{v}"))))
            .collect()
    }

    #[test]
    fn select_empty_is_error() {
        assert!(select("x", &[], None, None).is_err());
    }

    #[test]
    fn select_exact_version_match() {
        let v = vers(&["1.0.0", "1.2.0"]);
        let s = select("x", &v, Some("1.0.0"), None).unwrap();
        assert_eq!(s.path, PathBuf::from("/reg/crate-1.0.0"));
        assert!(s.warn.is_none());
    }

    #[test]
    fn select_exact_version_missing_lists_available_desc() {
        let v = vers(&["1.0.0", "1.2.0"]);
        let err = select("x", &v, Some("9.9.9"), None).unwrap_err();
        assert!(err.contains("not vendored"));
        // Highest first.
        assert!(err.contains("1.2.0, 1.0.0"), "got: {err}");
    }

    #[test]
    fn select_lock_pin_wins_over_highest() {
        let v = vers(&["1.0.0", "2.0.0"]);
        let s = select("x", &v, None, Some("1.0.0")).unwrap();
        assert_eq!(s.path, PathBuf::from("/reg/crate-1.0.0"));
        assert!(s.warn.is_none());
    }

    #[test]
    fn select_lock_pin_not_vendored_is_error() {
        let v = vers(&["1.0.0"]);
        let err = select("x", &v, None, Some("3.0.0")).unwrap_err();
        assert!(err.contains("Cargo.lock pins"));
    }

    #[test]
    fn select_no_pin_single_version_no_warning() {
        let v = vers(&["0.9.0"]);
        let s = select("x", &v, None, None).unwrap();
        assert_eq!(s.path, PathBuf::from("/reg/crate-0.9.0"));
        assert!(s.warn.is_none());
    }

    #[test]
    fn select_no_pin_multi_version_picks_highest_and_warns() {
        let v = vers(&["0.6.21", "1.0.0"]);
        let s = select("x", &v, None, None).unwrap();
        assert_eq!(s.path, PathBuf::from("/reg/crate-1.0.0"));
        let w = s.warn.expect("expected a warning");
        assert!(w.contains("1.0.0") && w.contains("highest"));
    }

    #[test]
    fn versions_of_matches_exact_name_not_same_prefix() {
        // Build a throwaway registry dir with sibling crates that share a
        // prefix, plus a stray file. `versions_of("serde")` must return only
        // serde's own version dirs — not `serde_json`, `serde-derive`, or a
        // non-version-suffixed dir.
        let root = std::env::temp_dir().join(format!("rmap-krate-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for d in [
            "serde-1.0.0",
            "serde-1.0.228",
            "serde_json-1.0.1", // underscore: different crate
            "serde-derive-1.0.0", // hyphen suffix: different crate
            "serde-notaversion", // remainder doesn't start with a digit
        ] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        fs::write(root.join("serde-9.9.9-afile"), "x").unwrap(); // a file, not a dir

        let mut got: Vec<String> = versions_of(&root, "serde")
            .into_iter()
            .map(|(v, _)| v)
            .collect();
        got.sort();
        assert_eq!(got, vec!["1.0.0".to_string(), "1.0.228".to_string()]);

        fs::remove_dir_all(&root).unwrap();
    }
}
