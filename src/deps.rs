//! File-level dependency graph from `use` and `mod` statements.
//!
//! Algorithm:
//!   1. Find a crate root (`src/main.rs` or `src/lib.rs`).
//!   2. Walk `mod x;` declarations to build a module tree, mapping each
//!      `.rs` file to its module path (`crate::a::b`).
//!   3. For each file, parse all `use` items, flatten `UseTree`, and
//!      resolve each path against the module tree:
//!        - `crate::*` → from crate root
//!        - `self::*`  → from current module
//!        - `super::*` → from parent module
//!        - other      → external crate (first segment)
//!   4. Emit edges: internal (file → file) and external (file → crate).
//!
//! Limitations (Tier 0):
//!   - Single-crate only. Workspace + `[[bin]]` overrides not parsed.
//!   - Inline `mod x { ... }` treated as a child whose file is the
//!     parent file (so `super::` from inside an inline mod resolves up
//!     correctly, but cross-references between inline mods in different
//!     files do not deduplicate).
//!   - Glob `use a::b::*;` resolves to the module's file (one edge).
//!   - Cfg-gated `mod`/`use` items are followed unconditionally.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use syn::{Item, UseTree};

#[derive(Debug, Clone, Copy)]
pub enum Mode {
    Forward,
    Reverse,
}

pub struct DepsOptions {
    pub focus: Option<PathBuf>,
    pub mode: Mode,
    pub include_external: bool,
    /// Optional repo-relative path prefix. When set, whole-repo rendering
    /// only emits rows whose file (forward) or target (reverse) lies
    /// under this prefix. Has no effect in focus mode.
    pub scope: Option<String>,
}

#[derive(Default)]
struct ModNode {
    file: PathBuf,
    children: BTreeMap<String, ModNode>,
}

struct ModTree {
    root: ModNode,
    /// Canonical absolute file path → module path (excluding `crate`).
    file_to_modpath: BTreeMap<PathBuf, Vec<String>>,
}

#[derive(Debug, Default)]
pub struct Graph {
    /// rel_path → set of internal rel_paths it imports via `use`
    pub out_internal: BTreeMap<String, BTreeSet<String>>,
    /// rel_path → set of external crate names it imports
    pub out_external: BTreeMap<String, BTreeSet<String>>,
    /// rel_path → set of child file rel_paths declared via `mod x;`.
    /// Distinct from `out_internal` because `mod` and `use` are
    /// semantically different. `deps` rendering ignores this; `graph`
    /// reachability uses it.
    pub mod_edges: BTreeMap<String, BTreeSet<String>>,
    /// All files seen (so isolated nodes still appear).
    pub files: BTreeSet<String>,
}

/// Build the dep graph for a repo. Public for `graph` lens.
/// Returns `None` if no crate root is found.
pub fn build_from_repo(repo: &Path) -> Option<Graph> {
    let canon_repo = fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf());
    let root_file = find_crate_root(&canon_repo)?;
    let tree = build_mod_tree(&root_file);
    Some(build_graph(&tree, &canon_repo))
}

/// Locate the detected crate root file (relative to repo).
pub fn detect_crate_root(repo: &Path) -> Option<String> {
    let canon = fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf());
    find_crate_root(&canon).map(|p| rel_to(&p, &canon))
}

pub fn run(repo_root: &Path, opts: &DepsOptions) -> String {
    let canon_repo = fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());

    let Some(root_file) = find_crate_root(&canon_repo) else {
        return format!(
            "no crate root found (looked for {}/src/main.rs and {}/src/lib.rs)\n",
            repo_root.display(),
            repo_root.display()
        );
    };

    let tree = build_mod_tree(&root_file);
    let graph = build_graph(&tree, &canon_repo);

    if let Some(focus) = &opts.focus {
        let focus_abs = fs::canonicalize(focus).unwrap_or_else(|_| focus.clone());
        let focus_rel = rel_to(&focus_abs, &canon_repo);
        return render_focus(&graph, &focus_rel, opts.include_external);
    }

    render_all(&graph, opts.mode, opts.include_external, opts.scope.as_deref())
}

// --- crate root + mod tree -------------------------------------------------

fn find_crate_root(repo: &Path) -> Option<PathBuf> {
    let main = repo.join("src/main.rs");
    if main.is_file() {
        return Some(main);
    }
    let lib = repo.join("src/lib.rs");
    if lib.is_file() {
        return Some(lib);
    }
    None
}

fn build_mod_tree(root_file: &Path) -> ModTree {
    let mut file_to_modpath: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
    let abs_root = fs::canonicalize(root_file).unwrap_or_else(|_| root_file.to_path_buf());
    file_to_modpath.insert(abs_root.clone(), Vec::new());
    let root = expand(&abs_root, &Vec::new(), &mut file_to_modpath);
    ModTree {
        root,
        file_to_modpath,
    }
}

fn expand(
    file: &Path,
    modpath: &[String],
    file_to_modpath: &mut BTreeMap<PathBuf, Vec<String>>,
) -> ModNode {
    let mut node = ModNode {
        file: file.to_path_buf(),
        children: BTreeMap::new(),
    };
    let Ok(src) = fs::read_to_string(file) else {
        return node;
    };
    let Ok(parsed) = syn::parse_file(&src) else {
        return node;
    };
    walk_items(&parsed.items, file, modpath, &mut node, file_to_modpath);
    node
}

fn walk_items(
    items: &[Item],
    file: &Path,
    modpath: &[String],
    parent: &mut ModNode,
    file_to_modpath: &mut BTreeMap<PathBuf, Vec<String>>,
) {
    for item in items {
        let Item::Mod(m) = item else {
            continue;
        };
        let child_name = m.ident.to_string();
        let mut child_modpath = modpath.to_vec();
        child_modpath.push(child_name.clone());

        if let Some((_, inline_items)) = &m.content {
            // Inline mod — same file, recurse for nested mod decls.
            let mut child_node = ModNode {
                file: file.to_path_buf(),
                children: BTreeMap::new(),
            };
            walk_items(
                inline_items,
                file,
                &child_modpath,
                &mut child_node,
                file_to_modpath,
            );
            parent.children.insert(child_name, child_node);
        } else if let Some(child_file) = resolve_mod_file(file, &child_name) {
            let abs_child = fs::canonicalize(&child_file).unwrap_or(child_file);
            file_to_modpath
                .entry(abs_child.clone())
                .or_insert_with(|| child_modpath.clone());
            let child_node = expand(&abs_child, &child_modpath, file_to_modpath);
            parent.children.insert(child_name, child_node);
        }
    }
}

fn resolve_mod_file(parent_file: &Path, child: &str) -> Option<PathBuf> {
    let parent_dir = parent_file.parent()?;
    let parent_stem = parent_file.file_name()?.to_string_lossy().into_owned();
    let search_dir = if matches!(parent_stem.as_str(), "lib.rs" | "main.rs" | "mod.rs") {
        parent_dir.to_path_buf()
    } else {
        let base = parent_file.file_stem()?.to_string_lossy().into_owned();
        parent_dir.join(base)
    };
    let direct = search_dir.join(format!("{child}.rs"));
    if direct.is_file() {
        return Some(direct);
    }
    let nested = search_dir.join(child).join("mod.rs");
    if nested.is_file() {
        return Some(nested);
    }
    None
}

// --- graph build -----------------------------------------------------------

fn build_graph(tree: &ModTree, repo: &Path) -> Graph {
    let mut g = Graph::default();
    // Always include every known file as a node, even with no edges.
    for abs in tree.file_to_modpath.keys() {
        g.files.insert(rel_to(abs, repo));
    }
    // Mod-decl edges from the tree.
    let mut mod_pairs: BTreeMap<PathBuf, BTreeSet<PathBuf>> = BTreeMap::new();
    collect_mod_edges(&tree.root, &mut mod_pairs);
    for (parent, children) in mod_pairs {
        let pr = rel_to(&parent, repo);
        for c in children {
            g.mod_edges
                .entry(pr.clone())
                .or_default()
                .insert(rel_to(&c, repo));
        }
    }
    for (abs, modpath) in &tree.file_to_modpath {
        let rel = rel_to(abs, repo);
        let Ok(src) = fs::read_to_string(abs) else {
            continue;
        };
        let Ok(parsed) = syn::parse_file(&src) else {
            continue;
        };
        // Each entry: (full use path, modpath at the use site).
        let mut paths: Vec<(Vec<String>, Vec<String>)> = Vec::new();
        collect_uses_with_modpath(&parsed.items, modpath, &mut paths);

        for (full, use_site_modpath) in paths {
            match resolve(&full, &use_site_modpath, &tree.root) {
                Some(Resolution::Internal(target_file)) => {
                    if target_file == *abs {
                        continue; // self-import (re-exports inside same file)
                    }
                    let target_rel = rel_to(&target_file, repo);
                    g.out_internal
                        .entry(rel.clone())
                        .or_default()
                        .insert(target_rel);
                }
                Some(Resolution::External(name)) => {
                    g.out_external
                        .entry(rel.clone())
                        .or_default()
                        .insert(name);
                }
                None => {}
            }
        }
    }
    g
}

/// Collect every `use` path along with the modpath it appears under.
/// Inline `mod x { ... }` extends the modpath so `super::` resolves
/// correctly from inside test modules and other inline submodules.
fn collect_mod_edges(node: &ModNode, out: &mut BTreeMap<PathBuf, BTreeSet<PathBuf>>) {
    for child in node.children.values() {
        if child.file != node.file {
            out.entry(node.file.clone())
                .or_default()
                .insert(child.file.clone());
        }
        collect_mod_edges(child, out);
    }
}

fn collect_uses_with_modpath(
    items: &[Item],
    modpath: &[String],
    out: &mut Vec<(Vec<String>, Vec<String>)>,
) {
    for item in items {
        match item {
            Item::Use(u) => {
                let mut paths = Vec::new();
                flatten_use(&u.tree, &mut Vec::new(), &mut paths);
                for p in paths {
                    out.push((p, modpath.to_vec()));
                }
            }
            Item::Mod(m) => {
                if let Some((_, inner)) = &m.content {
                    let mut child = modpath.to_vec();
                    child.push(m.ident.to_string());
                    collect_uses_with_modpath(inner, &child, out);
                }
            }
            _ => {}
        }
    }
}

fn flatten_use(tree: &UseTree, prefix: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
    match tree {
        UseTree::Path(p) => {
            prefix.push(p.ident.to_string());
            flatten_use(&p.tree, prefix, out);
            prefix.pop();
        }
        UseTree::Name(n) => {
            let mut full = prefix.clone();
            full.push(n.ident.to_string());
            out.push(full);
        }
        UseTree::Rename(r) => {
            let mut full = prefix.clone();
            full.push(r.ident.to_string());
            out.push(full);
        }
        UseTree::Glob(_) => {
            out.push(prefix.clone());
        }
        UseTree::Group(g) => {
            for child in &g.items {
                flatten_use(child, prefix, out);
            }
        }
    }
}

enum Resolution {
    Internal(PathBuf),
    External(String),
}

fn resolve(segments: &[String], current_modpath: &[String], root: &ModNode) -> Option<Resolution> {
    if segments.is_empty() {
        return None;
    }
    let (start, rest_idx): (&ModNode, usize) = match segments[0].as_str() {
        "crate" => (root, 1),
        "self" => {
            let n = navigate(root, current_modpath)?;
            (n, 1)
        }
        "super" => {
            // Count leading `super` chain.
            let mut up = 0usize;
            while up < segments.len() && segments[up] == "super" {
                up += 1;
            }
            if up > current_modpath.len() {
                return None;
            }
            let parent_path = &current_modpath[..current_modpath.len() - up];
            let n = navigate(root, parent_path)?;
            (n, up)
        }
        _ => {
            // External crate (or std/core/alloc/proc_macro).
            return Some(Resolution::External(segments[0].clone()));
        }
    };

    // Walk into the mod tree as far as segments match child mods.
    let mut node = start;
    for seg in &segments[rest_idx..] {
        if let Some(child) = node.children.get(seg) {
            node = child;
        } else {
            break;
        }
    }
    Some(Resolution::Internal(node.file.clone()))
}

fn navigate<'a>(root: &'a ModNode, modpath: &[String]) -> Option<&'a ModNode> {
    let mut node = root;
    for seg in modpath {
        node = node.children.get(seg)?;
    }
    Some(node)
}

// --- render ----------------------------------------------------------------

fn rel_to(abs: &Path, repo: &Path) -> String {
    abs.strip_prefix(repo)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| abs.to_string_lossy().replace('\\', "/"))
}

fn render_all(graph: &Graph, mode: Mode, include_ext: bool, scope: Option<&str>) -> String {
    let in_scope = |f: &str| match scope {
        None => true,
        Some(s) => path_under(f, s),
    };
    let scoped_files: Vec<&String> = graph.files.iter().filter(|f| in_scope(f)).collect();
    if scoped_files.is_empty() {
        return match scope {
            Some(s) => format!("no files under scope `{s}`\n"),
            None => String::new(),
        };
    }
    let max_w = scoped_files.iter().map(|s| s.len()).max().unwrap_or(0);

    let mut out = String::new();
    match mode {
        Mode::Forward => {
            for f in &scoped_files {
                let mut deps: Vec<String> = graph
                    .out_internal
                    .get(*f)
                    .into_iter()
                    .flatten()
                    .map(|s| short_name(s))
                    .collect();
                if include_ext {
                    if let Some(ext) = graph.out_external.get(*f) {
                        for e in ext {
                            deps.push(e.clone());
                        }
                    }
                }
                let rhs = if deps.is_empty() {
                    "-".to_string()
                } else {
                    deps.join(", ")
                };
                out.push_str(&format!("{:<w$} -> {}\n", *f, rhs, w = max_w));
            }
        }
        Mode::Reverse => {
            // Build reverse internal map (over the full graph; callers may
            // live outside scope).
            let mut rev: BTreeMap<&String, BTreeSet<&String>> = BTreeMap::new();
            for f in &graph.files {
                rev.entry(f).or_default();
            }
            for (src, tgts) in &graph.out_internal {
                for t in tgts {
                    rev.entry(t).or_default().insert(src);
                }
            }
            for f in &scoped_files {
                let callers = rev.get(*f).cloned().unwrap_or_default();
                let rhs = if callers.is_empty() {
                    "-".to_string()
                } else {
                    callers
                        .iter()
                        .map(|s| short_name(s))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                out.push_str(&format!("{:<w$} <- {}\n", *f, rhs, w = max_w));
            }
        }
    }
    out
}

/// Path-segment-aligned prefix match: `src/domain/x` is under `src/domain`
/// but not under `src/dom`.
fn path_under(file: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    file == prefix
        || file.starts_with(&format!("{prefix}/"))
}

fn render_focus(graph: &Graph, focus: &str, include_ext: bool) -> String {
    if !graph.files.contains(focus) {
        return format!("file `{focus}` not in module tree\n");
    }
    let mut out = String::new();
    out.push_str(focus);
    out.push('\n');

    let outs: Vec<String> = graph
        .out_internal
        .get(focus)
        .into_iter()
        .flatten()
        .cloned()
        .collect();
    out.push_str("  out: ");
    if outs.is_empty() {
        out.push('-');
    } else {
        out.push_str(&outs.join(", "));
    }
    out.push('\n');

    let mut callers: Vec<&String> = Vec::new();
    for (src, tgts) in &graph.out_internal {
        if tgts.contains(focus) {
            callers.push(src);
        }
    }
    callers.sort();
    out.push_str("  in:  ");
    if callers.is_empty() {
        out.push('-');
    } else {
        out.push_str(
            &callers
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    out.push('\n');

    if include_ext {
        let exts: Vec<String> = graph
            .out_external
            .get(focus)
            .into_iter()
            .flatten()
            .cloned()
            .collect();
        out.push_str("  ext: ");
        if exts.is_empty() {
            out.push('-');
        } else {
            out.push_str(&exts.join(", "));
        }
        out.push('\n');
    }
    out
}

fn short_name(rel: &str) -> String {
    Path::new(rel)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| rel.to_string())
}

// --- tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(dir: &Path, rel: &str, content: &str) -> PathBuf {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, content).unwrap();
        p
    }

    fn tmp() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "rmap-deps-test-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            N.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn flatten_use_handles_groups_and_globs() {
        let src = "use a::{b::c, d::*, e as f};";
        let parsed = syn::parse_file(src).unwrap();
        let mut paths = Vec::new();
        for item in &parsed.items {
            if let Item::Use(u) = item {
                flatten_use(&u.tree, &mut Vec::new(), &mut paths);
            }
        }
        let joined: Vec<String> = paths
            .iter()
            .map(|p| p.join("::"))
            .collect();
        assert!(joined.contains(&"a::b::c".to_string()));
        assert!(joined.contains(&"a::d".to_string()), "{:?}", joined);
        assert!(joined.contains(&"a::e".to_string()));
    }

    #[test]
    fn resolves_crate_self_super_external() {
        let dir = tmp();
        write(
            &dir,
            "src/main.rs",
            "mod a; mod b; use crate::a::Foo; use std::io;",
        );
        write(
            &dir,
            "src/a.rs",
            "use crate::b::Bar; use super::b::Baz; use self::sub::X; mod sub { pub struct X; }",
        );
        write(&dir, "src/b.rs", "pub struct Bar; pub struct Baz;");

        let opts = DepsOptions {
            focus: None,
            mode: Mode::Forward,
            include_external: true,
            scope: None,
        };
        let out = run(&dir, &opts);
        // a.rs imports b.rs (via crate:: and super::)
        assert!(out.contains("src/a.rs"), "{out}");
        assert!(out.contains("src/b.rs"), "{out}");
        // main.rs has external `std`
        let main_line = out.lines().find(|l| l.starts_with("src/main.rs")).unwrap();
        assert!(main_line.contains("std"), "{main_line}");
        assert!(main_line.contains("a"), "{main_line}");
    }

    #[test]
    fn reverse_mode_lists_callers() {
        let dir = tmp();
        write(&dir, "src/main.rs", "mod a; mod b; use crate::a::X;");
        write(&dir, "src/a.rs", "use crate::b::Y; pub struct X;");
        write(&dir, "src/b.rs", "pub struct Y;");

        let opts = DepsOptions {
            focus: None,
            mode: Mode::Reverse,
            include_external: false,
            scope: None,
        };
        let out = run(&dir, &opts);
        let b_line = out.lines().find(|l| l.starts_with("src/b.rs")).unwrap();
        assert!(b_line.contains("a"), "{b_line}");
    }

    #[test]
    fn focus_mode_shows_in_out_ext() {
        let dir = tmp();
        write(&dir, "src/main.rs", "mod a; mod b; use crate::a::X;");
        write(&dir, "src/a.rs", "use crate::b::Y; use std::io; pub struct X;");
        write(&dir, "src/b.rs", "pub struct Y;");

        let opts = DepsOptions {
            focus: Some(dir.join("src/a.rs")),
            mode: Mode::Forward,
            include_external: true,
            scope: None,
        };
        let out = run(&dir, &opts);
        assert!(out.contains("out: src/b.rs"), "{out}");
        assert!(out.contains("in:  src/main.rs"), "{out}");
        assert!(out.contains("ext: std"), "{out}");
    }

    #[test]
    fn no_crate_root_returns_message() {
        let dir = tmp();
        let opts = DepsOptions {
            focus: None,
            mode: Mode::Forward,
            include_external: false,
            scope: None,
        };
        let out = run(&dir, &opts);
        assert!(out.contains("no crate root"), "{out}");
    }

    #[test]
    fn handles_mod_dot_rs_layout() {
        let dir = tmp();
        write(&dir, "src/main.rs", "mod a;");
        write(&dir, "src/a/mod.rs", "mod b; use crate::a::b::X;");
        write(&dir, "src/a/b.rs", "pub struct X;");

        let opts = DepsOptions {
            focus: None,
            mode: Mode::Forward,
            include_external: false,
            scope: None,
        };
        let out = run(&dir, &opts);
        let a_line = out.lines().find(|l| l.starts_with("src/a/mod.rs")).unwrap();
        assert!(a_line.contains("b"), "{a_line}");
    }

    #[test]
    fn scope_filters_to_subtree() {
        let dir = tmp();
        write(&dir, "Cargo.toml", "[package]\nname = \"x\"\nversion = \"0.1.0\"\nedition = \"2021\"\n");
        write(&dir, "src/main.rs", "mod a; mod b;");
        write(&dir, "src/a.rs", "pub struct A;");
        write(&dir, "src/b.rs", "use crate::a::A;");

        let opts = DepsOptions {
            focus: None,
            mode: Mode::Forward,
            include_external: false,
            scope: Some("src/a.rs".into()),
        };
        let out = run(&dir, &opts);
        assert!(out.contains("src/a.rs"), "{out}");
        assert!(!out.contains("src/main.rs"), "{out}");
        assert!(!out.contains("src/b.rs"), "{out}");
    }

    #[test]
    fn path_under_segment_aligned() {
        assert!(path_under("src/domain/x.rs", "src/domain"));
        assert!(path_under("src/domain", "src/domain"));
        assert!(!path_under("src/domain_other/x.rs", "src/domain"));
        assert!(path_under("src/domain/x.rs", "src/domain/"));
    }
}
