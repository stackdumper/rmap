//! Path enumeration: build an in-memory tree of files/dirs rooted at PATH.
//!
//! Strategy: use the `ignore` crate's parallel walker. It respects
//! `.gitignore`, `.ignore`, global git excludes, and hidden-file rules
//! whether or not the root is inside a git work tree. No external `git`
//! process required.

use ignore::WalkBuilder;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum Node {
    Dir {
        name: String,
        rel: PathBuf,
        children: Vec<Node>,
    },
    File {
        name: String,
        abs: PathBuf,
    },
}

impl Node {
    pub fn name(&self) -> &str {
        match self {
            Node::Dir { name, .. } | Node::File { name, .. } => name,
        }
    }
}

/// Filter applied to enumerated paths.
#[derive(Default)]
pub struct Filter {
    /// Skip paths whose relative path contains any of these substrings.
    pub exclude: Vec<String>,
    /// If non-empty, only include files whose extension matches one of these
    /// (without leading `.`).
    pub ext: Vec<String>,
}

/// Build the tree for the given root. Returns `Err` with a printable
/// message if the root cannot be canonicalized.
pub fn enumerate(root: &Path, filter: &Filter) -> Result<Node, String> {
    let abs_root = fs::canonicalize(root)
        .map_err(|e| format!("error: cannot canonicalize {}: {e}", root.display()))?;
    let name = abs_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(".")
        .to_string();

    let paths = list_paths(&abs_root);
    let filtered: Vec<String> = paths
        .into_iter()
        .filter(|p| !filter.exclude.iter().any(|x| p.contains(x.as_str())))
        .filter(|p| ext_matches(p, &filter.ext))
        .collect();

    let mut root_node = TreeBuild::default();
    for rel in &filtered {
        root_node.insert(rel);
    }
    Ok(root_node.into_node(name, PathBuf::new(), &abs_root))
}

/// Find all directories under `root` whose path ends in `suffix` as a
/// path-segment-aligned suffix. Used by `module` to resolve fuzzy paths.
pub fn find_dirs_matching_suffix(root: &Path, suffix: &str) -> Vec<PathBuf> {
    let abs_root = match fs::canonicalize(root) {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };
    let paths = list_paths(&abs_root);
    let mut dirs: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for p in &paths {
        let mut cur = p.as_str();
        while let Some(idx) = cur.rfind('/') {
            cur = &cur[..idx];
            if !cur.is_empty() {
                dirs.insert(cur.to_string());
            }
        }
    }
    let suffix = suffix.trim_matches('/');
    dirs.into_iter()
        .filter(|d| d == suffix || d.ends_with(&format!("/{suffix}")))
        .map(|d| root.join(&d))
        .collect()
}

fn ext_matches(rel: &str, allowed: &[String]) -> bool {
    if allowed.is_empty() {
        return true;
    }
    let name = rel.rsplit('/').next().unwrap_or(rel);
    let Some(idx) = name.rfind('.') else {
        return false;
    };
    let ext = &name[idx + 1..];
    allowed.iter().any(|a| a.eq_ignore_ascii_case(ext))
}

#[derive(Default)]
struct TreeBuild {
    dirs: BTreeMap<String, TreeBuild>,
    files: Vec<String>,
}

impl TreeBuild {
    fn insert(&mut self, rel: &str) {
        let parts: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
        if parts.is_empty() {
            return;
        }
        let (file, dirs) = parts.split_last().unwrap();
        let mut node = self;
        for d in dirs {
            node = node.dirs.entry((*d).to_string()).or_default();
        }
        node.files.push((*file).to_string());
    }

    fn into_node(mut self, name: String, rel: PathBuf, abs_root: &Path) -> Node {
        let mut children: Vec<Node> = Vec::new();
        for (dname, dnode) in std::mem::take(&mut self.dirs) {
            let drel = rel.join(&dname);
            children.push(dnode.into_node(dname, drel, abs_root));
        }
        self.files.sort();
        for fname in self.files {
            let abs = abs_root.join(rel.join(&fname));
            children.push(Node::File { name: fname, abs });
        }
        Node::Dir {
            name,
            rel,
            children,
        }
    }
}

fn list_paths(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let walker = WalkBuilder::new(root)
        .hidden(false) // include dotfiles unless explicitly ignored
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(true)
        .parents(true)
        .filter_entry(|e| {
            // Skip VCS metadata dirs even when `hidden(false)`.
            !matches!(e.file_name().to_str(), Some(".git" | ".hg" | ".svn"))
        })
        .build();
    for entry in walker.flatten() {
        let path = entry.path();
        if path == root {
            continue;
        }
        let Ok(file_type) = entry.file_type().ok_or(()) else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if rel_str.is_empty() {
            continue;
        }
        out.push(rel_str);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ext_matches_empty_allowlist_passes_everything() {
        assert!(ext_matches("a/b.rs", &[]));
        assert!(ext_matches("anything", &[]));
    }

    #[test]
    fn ext_matches_filters_by_extension_case_insensitive() {
        let allow = vec!["rs".to_string(), "md".to_string()];
        assert!(ext_matches("src/a.rs", &allow));
        assert!(ext_matches("docs/x.MD", &allow));
        assert!(!ext_matches("Cargo.toml", &allow));
        assert!(!ext_matches("README", &allow));
    }
}
