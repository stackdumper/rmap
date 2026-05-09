//! Reachability subgraph from an entry file.
//!
//! Reuses `deps::Graph`. Walks forward (what does `entry` reach?) or
//! reverse (what reaches `entry`?) up to an optional depth limit.
//!
//! Default output is a single-line brace tree matching `rmap tree`
//! aesthetic. File names are shortened to file stems (so `src/walk.rs`
//! → `walk`); `mod.rs` / `lib.rs` / `main.rs` are prefixed with their
//! parent dir to disambiguate.
//!
//! Markers: `*` after a name = already shown elsewhere (revisit, edge
//! follows but children are elided); `~` = back-edge (cycle in current
//! DFS path); `{…}` = depth limit reached but children exist.
//!
//! `--mermaid` swaps to a `graph TD` block of deduped `a --> b` edges
//! for visualization.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use crate::deps::{self, Graph};

#[derive(Debug, Clone, Copy)]
pub enum Direction {
    Forward,
    Reverse,
}

pub struct GraphOptions {
    pub entry: Option<PathBuf>,
    pub direction: Direction,
    pub mermaid: bool,
    pub depth: Option<usize>,
    pub include_external: bool,
}

pub fn run(repo_root: &Path, opts: &GraphOptions) -> String {
    let Some(graph) = deps::build_from_repo(repo_root) else {
        return format!(
            "no crate root found (looked for {}/src/main.rs and {}/src/lib.rs)\n",
            repo_root.display(),
            repo_root.display()
        );
    };

    let entry = match resolve_entry(opts.entry.as_deref(), repo_root, &graph) {
        Ok(e) => e,
        Err(msg) => return msg,
    };

    let adj = build_adjacency(&graph, opts.direction, opts.include_external);

    if opts.mermaid {
        render_mermaid(&entry, &adj, opts.depth)
    } else {
        render_brace(&entry, &adj, opts.depth)
    }
}

fn resolve_entry(
    entry: Option<&Path>,
    repo: &Path,
    graph: &Graph,
) -> Result<String, String> {
    if let Some(e) = entry {
        // Try a few normalizations: absolute -> rel; bare path; canonical.
        let candidates = entry_candidates(e, repo);
        for c in &candidates {
            if graph.files.contains(c) {
                return Ok(c.clone());
            }
        }
        return Err(format!(
            "entry `{}` not found in graph. Tried: {}\n",
            e.display(),
            candidates.join(", ")
        ));
    }
    deps::detect_crate_root(repo).ok_or_else(|| {
        format!(
            "no crate root found (looked for {}/src/main.rs and {}/src/lib.rs)\n",
            repo.display(),
            repo.display()
        )
    })
}

fn entry_candidates(e: &Path, repo: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let raw = e.to_string_lossy().replace('\\', "/");
    out.push(raw.trim_start_matches("./").to_string());
    if let Ok(canon_e) = std::fs::canonicalize(e) {
        if let Ok(canon_repo) = std::fs::canonicalize(repo) {
            if let Ok(stripped) = canon_e.strip_prefix(&canon_repo) {
                out.push(stripped.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    out
}

/// Adjacency list: file → list of out-edges (file or `ext:<crate>`).
type Adj = BTreeMap<String, Vec<String>>;

fn build_adjacency(graph: &Graph, dir: Direction, include_ext: bool) -> Adj {
    let mut adj: Adj = BTreeMap::new();
    for f in &graph.files {
        adj.entry(f.clone()).or_default();
    }
    match dir {
        Direction::Forward => {
            for (src, tgts) in &graph.out_internal {
                let entry = adj.entry(src.clone()).or_default();
                for t in tgts {
                    entry.push(t.clone());
                }
            }
            // Mod edges: `mod x;` reaches the child file even without `use`.
            for (src, tgts) in &graph.mod_edges {
                let entry = adj.entry(src.clone()).or_default();
                for t in tgts {
                    entry.push(t.clone());
                }
            }
            if include_ext {
                for (src, exts) in &graph.out_external {
                    let entry = adj.entry(src.clone()).or_default();
                    for e in exts {
                        entry.push(format!("ext:{e}"));
                    }
                }
            }
        }
        Direction::Reverse => {
            for (src, tgts) in &graph.out_internal {
                for t in tgts {
                    adj.entry(t.clone()).or_default().push(src.clone());
                }
            }
            for (src, tgts) in &graph.mod_edges {
                for t in tgts {
                    adj.entry(t.clone()).or_default().push(src.clone());
                }
            }
            // External nodes have no inbound from internal in this model;
            // ignore --ext in reverse mode.
        }
    }
    // Sort + dedupe each entry for stable output.
    for v in adj.values_mut() {
        v.sort();
        v.dedup();
    }
    adj
}

// --- brace render ----------------------------------------------------------

fn render_brace(entry: &str, adj: &Adj, max_depth: Option<usize>) -> String {
    let mut out = String::new();
    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut stack: BTreeSet<String> = BTreeSet::new();
    walk_brace(entry, adj, 0, max_depth, &mut visited, &mut stack, &mut out);
    out.push('\n');
    out
}

fn walk_brace(
    node: &str,
    adj: &Adj,
    cur_depth: usize,
    max_depth: Option<usize>,
    visited: &mut BTreeSet<String>,
    stack: &mut BTreeSet<String>,
    out: &mut String,
) {
    let label = short_label(node);

    // Cycle (back-edge in current DFS path) takes precedence.
    if stack.contains(node) {
        out.push_str(&label);
        out.push('~');
        return;
    }
    if !visited.insert(node.to_string()) {
        out.push_str(&label);
        out.push('*');
        return;
    }

    out.push_str(&label);

    let has_children = adj.get(node).map(|c| !c.is_empty()).unwrap_or(false);
    if !has_children {
        return;
    }
    if matches!(max_depth, Some(m) if cur_depth >= m) {
        out.push_str(" {…}");
        return;
    }

    stack.insert(node.to_string());
    out.push_str(" { ");
    let children = &adj[node];
    for (i, c) in children.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        walk_brace(c, adj, cur_depth + 1, max_depth, visited, stack, out);
    }
    out.push_str(" }");
    stack.remove(node);
}

/// Compact label: file stem only. Disambiguate `mod.rs` / `lib.rs` /
/// `main.rs` by prefixing with parent dir name. `ext:foo` passes through.
fn short_label(rel: &str) -> String {
    if let Some(name) = rel.strip_prefix("ext:") {
        return format!("ext:{name}");
    }
    let p = Path::new(rel);
    let stem = p
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(rel)
        .to_string();
    if matches!(stem.as_str(), "mod" | "lib" | "main") {
        if let Some(parent) = p
            .parent()
            .and_then(|pp| pp.file_name())
            .and_then(|s| s.to_str())
        {
            return format!("{parent}/{stem}");
        }
    }
    stem
}

// --- mermaid render --------------------------------------------------------

fn render_mermaid(entry: &str, adj: &Adj, max_depth: Option<usize>) -> String {
    let mut edges: BTreeSet<(String, String)> = BTreeSet::new();
    let mut nodes: BTreeSet<String> = BTreeSet::new();
    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut q: VecDeque<(String, usize)> = VecDeque::new();
    q.push_back((entry.to_string(), 0));
    while let Some((n, d)) = q.pop_front() {
        if !visited.insert(n.clone()) {
            continue;
        }
        nodes.insert(n.clone());
        if matches!(max_depth, Some(m) if d >= m) {
            continue;
        }
        if let Some(children) = adj.get(&n) {
            for c in children {
                edges.insert((n.clone(), c.clone()));
                nodes.insert(c.clone());
                if !visited.contains(c) {
                    q.push_back((c.clone(), d + 1));
                }
            }
        }
    }

    let mut out = String::from("graph TD\n");
    // Declare each node once with its label, then bare IDs in edges.
    for n in &nodes {
        out.push_str(&format!("  {}[\"{}\"]\n", mermaid_id(n), short_label(n)));
    }
    for (a, b) in &edges {
        out.push_str(&format!("  {} --> {}\n", mermaid_id(a), mermaid_id(b)));
    }
    out
}

/// Safe node ID: alphanumeric only, leading char prefixed if needed.
fn mermaid_id(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    safe
}

// --- tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn graph_from(edges: &[(&str, &str)], files: &[&str]) -> Graph {
        let mut g = Graph::default();
        for f in files {
            g.files.insert((*f).to_string());
        }
        for (a, b) in edges {
            g.files.insert((*a).to_string());
            g.files.insert((*b).to_string());
            g.out_internal
                .entry((*a).to_string())
                .or_default()
                .insert((*b).to_string());
        }
        g
    }

    #[test]
    fn brace_walks_forward_only_reachable() {
        let g = graph_from(
            &[("a", "b"), ("a", "c"), ("b", "d"), ("orphan", "x")],
            &["a", "b", "c", "d", "orphan", "x"],
        );
        let adj = build_adjacency(&g, Direction::Forward, false);
        let out = render_brace("a", &adj, None);
        assert_eq!(out.trim(), "a { b { d }, c }");
        assert!(!out.contains("orphan"));
    }

    #[test]
    fn brace_marks_cycle_with_tilde() {
        let g = graph_from(&[("a", "b"), ("b", "a")], &["a", "b"]);
        let adj = build_adjacency(&g, Direction::Forward, false);
        let out = render_brace("a", &adj, None);
        assert!(out.contains("a~"), "{out}");
    }

    #[test]
    fn brace_marks_revisit_with_star() {
        let g = graph_from(
            &[("a", "b"), ("a", "c"), ("b", "d"), ("c", "d")],
            &["a", "b", "c", "d"],
        );
        let adj = build_adjacency(&g, Direction::Forward, false);
        let out = render_brace("a", &adj, None);
        // First `d` expanded, second occurs as `d*`.
        assert!(out.contains("d*"), "{out}");
    }

    #[test]
    fn brace_depth_limit_uses_ellipsis() {
        let g = graph_from(&[("a", "b"), ("b", "c"), ("c", "d")], &["a", "b", "c", "d"]);
        let adj = build_adjacency(&g, Direction::Forward, false);
        let out = render_brace("a", &adj, Some(1));
        assert!(out.contains("b {…}"), "{out}");
        assert!(!out.contains("c"), "{out}");
    }

    #[test]
    fn reverse_walks_callers() {
        let g = graph_from(
            &[("main", "a"), ("main", "b"), ("a", "util"), ("b", "util")],
            &["main", "a", "b", "util"],
        );
        let adj = build_adjacency(&g, Direction::Reverse, false);
        let out = render_brace("util", &adj, None);
        assert!(out.contains("util"));
        assert!(out.contains("a"));
        assert!(out.contains("b"));
        assert!(out.contains("main"));
    }

    #[test]
    fn mermaid_dedupes_node_declarations() {
        let g = graph_from(&[("a", "b"), ("a", "c"), ("c", "b")], &["a", "b", "c"]);
        let adj = build_adjacency(&g, Direction::Forward, false);
        let out = render_mermaid("a", &adj, None);
        assert!(out.starts_with("graph TD\n"), "{out}");
        // `b` declared exactly once.
        assert_eq!(out.matches("b[\"b\"]").count(), 1, "{out}");
        // Edges use bare IDs.
        assert!(out.contains("a --> b"));
        assert!(out.contains("a --> c"));
        assert!(out.contains("c --> b"));
    }

    #[test]
    fn ext_nodes_included_when_flag_set() {
        let mut g = graph_from(&[], &["a"]);
        g.out_external
            .entry("a".to_string())
            .or_default()
            .insert("std".to_string());
        let adj = build_adjacency(&g, Direction::Forward, true);
        let out = render_brace("a", &adj, None);
        assert!(out.contains("ext:std"), "{out}");
    }

    #[test]
    fn ext_nodes_skipped_by_default() {
        let mut g = graph_from(&[], &["a"]);
        g.out_external
            .entry("a".to_string())
            .or_default()
            .insert("std".to_string());
        let adj = build_adjacency(&g, Direction::Forward, false);
        let out = render_brace("a", &adj, None);
        assert!(!out.contains("ext:std"), "{out}");
    }

    #[test]
    fn short_label_disambiguates_mod_files() {
        assert_eq!(short_label("src/foo/mod.rs"), "foo/mod");
        assert_eq!(short_label("src/lib.rs"), "src/lib");
        assert_eq!(short_label("src/walk.rs"), "walk");
        assert_eq!(short_label("ext:syn"), "ext:syn");
    }
}
