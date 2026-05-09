//! Render `walk::Node` trees as brace text.
//!
//! Brace tree layout is one of three shapes per directory:
//!   - **ellipsis**:  beyond `--depth`, collapse to `{ ... N subdirs, M files (depth cap) }`.
//!   - **inline**:    no subdirs and no per-file detail expansion → `name { a, b, c }`.
//!   - **multi-line**: subdirs present, or `--detail` expands `.rs` items per file.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use crate::parse::{self, ParseOptions};
use crate::walk::Node;

#[derive(Default)]
pub struct TreeOptions {
    pub depth: Option<usize>,
    pub detail: bool,
    pub lines: bool,
    pub caps: Vec<(String, usize)>,
}

pub fn tree(node: &Node, opts: &TreeOptions) -> String {
    let mut out = String::new();
    render(node, 0, opts, &mut out);
    out
}

fn render(node: &Node, depth: usize, opts: &TreeOptions, out: &mut String) {
    match node {
        Node::Dir {
            name,
            rel,
            children,
        } => render_dir(name, rel, children, depth, opts, out),
        Node::File { .. } => {
            // Top-level file (rare: explicit `tree path/to/file.rs`).
            let _ = writeln!(out, "{}{}", indent(depth), file_label(node, opts));
        }
    }
}

fn render_dir(
    name: &str,
    rel: &Path,
    children: &[Node],
    depth: usize,
    opts: &TreeOptions,
    out: &mut String,
) {
    let depth_exceeded = matches!(opts.depth, Some(d) if depth >= d);
    if depth_exceeded {
        render_ellipsis(name, depth, children, out);
        return;
    }

    let (dirs, files_all) = partition(children);
    let (files, truncated) = apply_cap(files_all, rel, &opts.caps);
    let detail_expands = opts.detail && files.iter().any(|f| f.name().ends_with(".rs"));

    if dirs.is_empty() && !detail_expands {
        render_inline(name, depth, &files, truncated.as_deref(), opts, out);
    } else {
        render_multiline(name, depth, &dirs, &files, truncated.as_deref(), opts, out);
    }
}

fn render_ellipsis(name: &str, depth: usize, children: &[Node], out: &mut String) {
    let (n_dirs, n_files) = count(children);
    let prefix = indent(depth);
    if n_dirs + n_files == 0 {
        let _ = writeln!(out, "{prefix}{name} {{}}");
    } else {
        let _ = writeln!(
            out,
            "{prefix}{name} {{ ... {n_dirs} subdirs, {n_files} files (depth cap) }}"
        );
    }
}

fn render_inline(
    name: &str,
    depth: usize,
    files: &[&Node],
    truncated: Option<&str>,
    opts: &TreeOptions,
    out: &mut String,
) {
    let prefix = indent(depth);
    if truncated.is_none() && files.is_empty() {
        let _ = writeln!(out, "{prefix}{name} {{}}");
        return;
    }
    let mut parts: Vec<String> = Vec::new();
    if let Some(note) = truncated {
        parts.push(note.to_string());
    }
    parts.extend(files.iter().map(|f| file_label(f, opts)));
    let _ = writeln!(out, "{prefix}{name} {{ {} }}", parts.join(", "));
}

fn render_multiline(
    name: &str,
    depth: usize,
    dirs: &[&Node],
    files: &[&Node],
    truncated: Option<&str>,
    opts: &TreeOptions,
    out: &mut String,
) {
    let prefix = indent(depth);
    let inner = indent(depth + 1);
    let _ = writeln!(out, "{prefix}{name} {{");
    if let Some(note) = truncated {
        let _ = writeln!(out, "{inner}{note}");
    }
    for f in files {
        render_file_in_multiline(f, &inner, opts, out);
    }
    for d in dirs {
        render(d, depth + 1, opts, out);
    }
    let _ = writeln!(out, "{prefix}}}");
}

fn render_file_in_multiline(node: &Node, inner: &str, opts: &TreeOptions, out: &mut String) {
    let Node::File { name, abs, .. } = node else {
        return;
    };
    let label = file_label(node, opts);
    if opts.detail && name.ends_with(".rs") {
        match parse::render_file(abs, ParseOptions { lines: opts.lines }) {
            Some(items) => {
                let _ = writeln!(out, "{inner}{label} {{ {items} }}");
            }
            None => {
                let _ = writeln!(out, "{inner}{label}");
            }
        }
    } else {
        let _ = writeln!(out, "{inner}{label}");
    }
}

// --- helpers --------------------------------------------------------------

fn indent(depth: usize) -> String {
    "  ".repeat(depth)
}

fn partition(children: &[Node]) -> (Vec<&Node>, Vec<&Node>) {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for c in children {
        match c {
            Node::Dir { .. } => dirs.push(c),
            Node::File { .. } => files.push(c),
        }
    }
    (dirs, files)
}

fn count(children: &[Node]) -> (usize, usize) {
    let mut d = 0;
    let mut f = 0;
    for c in children {
        match c {
            Node::Dir { .. } => d += 1,
            Node::File { .. } => f += 1,
        }
    }
    (d, f)
}

fn apply_cap<'a>(
    mut files: Vec<&'a Node>,
    rel: &Path,
    caps: &[(String, usize)],
) -> (Vec<&'a Node>, Option<String>) {
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    let n = caps.iter().find_map(|(p, n)| (rel_str == *p).then_some(*n));
    let Some(n) = n else { return (files, None) };
    if files.len() <= n {
        return (files, None);
    }
    let dropped = files.len() - n;
    files = files.split_off(files.len() - n);
    (files, Some(format!("... {dropped} files truncated ...")))
}

fn file_label(node: &Node, opts: &TreeOptions) -> String {
    if !opts.lines {
        return node.name().to_string();
    }
    let Node::File { abs, name, .. } = node else {
        return node.name().to_string();
    };
    let n = count_lines(abs);
    format!("{name}:{n}")
}

fn count_lines(path: &Path) -> usize {
    fs::read_to_string(path)
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

// --- single-file module ---------------------------------------------------

/// Render a single file as `name { items }` (or `name {}` on parse failure /
/// non-Rust). Used when `module` is given a file path instead of a directory.
pub fn file_module(path: &Path, lines: bool) -> String {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("?")
        .to_string();
    let label = if lines {
        format!("{name}:{}", count_lines(path))
    } else {
        name.clone()
    };
    if !name.ends_with(".rs") {
        return format!("{label}\n");
    }
    match parse::render_file(path, ParseOptions { lines }) {
        Some(items) => format!("{label} {{ {items} }}\n"),
        None => format!("{label} {{}}\n"),
    }
}

// --- tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::walk::Node;
    use std::path::PathBuf;

    fn dir(name: &str, rel: &str, children: Vec<Node>) -> Node {
        Node::Dir {
            name: name.into(),
            rel: PathBuf::from(rel),
            children,
        }
    }
    fn file(name: &str, _rel: &str) -> Node {
        Node::File {
            name: name.into(),
            abs: PathBuf::from("/dev/null"),
        }
    }

    fn opts() -> TreeOptions {
        TreeOptions::default()
    }

    #[test]
    fn empty_dir_renders_empty_braces() {
        let n = dir("root", "", vec![]);
        assert_eq!(tree(&n, &opts()), "root {}\n");
    }

    #[test]
    fn pure_file_dir_collapses_to_inline() {
        let n = dir(
            "pkg",
            "pkg",
            vec![file("a.rs", "pkg/a.rs"), file("b.rs", "pkg/b.rs")],
        );
        assert_eq!(tree(&n, &opts()), "pkg { a.rs, b.rs }\n");
    }

    #[test]
    fn nested_dirs_render_multiline() {
        let n = dir(
            "root",
            "",
            vec![
                file("top.md", "top.md"),
                dir("sub", "sub", vec![file("x.rs", "sub/x.rs")]),
            ],
        );
        assert_eq!(tree(&n, &opts()), "root {\n  top.md\n  sub { x.rs }\n}\n");
    }

    #[test]
    fn depth_cap_collapses_to_ellipsis() {
        let n = dir(
            "root",
            "",
            vec![dir(
                "sub",
                "sub",
                vec![file("x.rs", "sub/x.rs"), file("y.rs", "sub/y.rs")],
            )],
        );
        let opts = TreeOptions {
            depth: Some(1),
            ..Default::default()
        };
        let s = tree(&n, &opts);
        assert!(
            s.contains("sub { ... 0 subdirs, 2 files (depth cap) }"),
            "{s}"
        );
    }

    #[test]
    fn cap_truncates_files_keeping_tail() {
        let n = dir(
            "sessions",
            "docs/sessions",
            vec![
                file("a.md", "docs/sessions/a.md"),
                file("b.md", "docs/sessions/b.md"),
                file("c.md", "docs/sessions/c.md"),
                file("d.md", "docs/sessions/d.md"),
            ],
        );
        let opts = TreeOptions {
            caps: vec![("docs/sessions".into(), 2)],
            ..Default::default()
        };
        let s = tree(&n, &opts);
        assert_eq!(s, "sessions { ... 2 files truncated ..., c.md, d.md }\n");
    }

    #[test]
    fn cap_only_matches_exact_relpath() {
        let n = dir(
            "sub",
            "other/sub",
            vec![
                file("a.md", "other/sub/a.md"),
                file("b.md", "other/sub/b.md"),
            ],
        );
        let opts = TreeOptions {
            caps: vec![("docs/sessions".into(), 1)],
            ..Default::default()
        };
        let s = tree(&n, &opts);
        assert_eq!(s, "sub { a.md, b.md }\n");
    }
}
