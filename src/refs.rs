//! Find references to a Rust identifier across the repo.
//!
//! Matches by trailing path segment (last `::` component). No full name
//! resolution: a hit on `foo` matches every `foo` regardless of which
//! `foo` it actually resolves to. Disambiguate by reading the surrounding
//! file, or by `--path` to scope the search.
//!
//! Output: one hit per line.
//!   `<file>:<line>:<col> def <kind> <name>`
//!   `<file>:<line>:<col> use <kind> <name>`
//!
//! `def` kinds: `fn`, `method`, `struct`, `enum`, `union`, `trait`,
//!              `const`, `static`, `type`, `macro`.
//! `use` kinds: `call`, `method`, `type`, `struct-lit`, `path`, `macro`,
//!              `import`, `pat`.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use proc_macro2::Span;
use syn::visit::{self, Visit};
use syn::{
    Expr, ExprCall, ExprMethodCall, ExprPath, ExprStruct, ImplItem, Item, ItemImpl, Macro, Pat,
    PatPath, PatStruct, PatTupleStruct, TraitItem, Type, TypePath, UseName, UsePath, UseRename,
};

use crate::walk::{self, Filter, Node};

#[derive(Debug, Clone, Copy)]
pub enum Mode {
    Both,
    DefsOnly,
    UsesOnly,
}

pub struct RefsOptions {
    pub name: String,
    pub mode: Mode,
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
struct Hit {
    file: String,
    line: usize,
    col: usize,
    role: &'static str, // "def" | "use"
    kind: &'static str,
    name: String,
}

pub fn run(root: &Path, opts: &RefsOptions) -> String {
    let mut files: Vec<(PathBuf, String)> = Vec::new();
    if root.is_file() {
        if root.extension().and_then(|e| e.to_str()) == Some("rs") {
            let abs = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
            let rel = display_rel(&abs);
            files.push((abs, rel));
        }
    } else {
        let filter = Filter {
            exclude: Vec::new(),
            ext: vec!["rs".to_string()],
        };
        let tree = walk::enumerate(root, &filter);
        collect_rs_files(&tree, &mut files);
    }

    let mut hits: BTreeSet<Hit> = BTreeSet::new();
    for (abs, rel) in &files {
        scan_file(abs, rel, &opts.name, opts.mode, &mut hits);
    }

    if hits.is_empty() {
        return format!("no hits for `{}`\n", opts.name);
    }
    let mut out = String::new();
    for h in &hits {
        out.push_str(&format!(
            "{}:{}:{} {} {} {}\n",
            h.file, h.line, h.col, h.role, h.kind, h.name
        ));
    }
    out
}

fn collect_rs_files(node: &Node, out: &mut Vec<(PathBuf, String)>) {
    match node {
        Node::Dir { children, .. } => {
            for c in children {
                collect_rs_files(c, out);
            }
        }
        Node::File { name, abs } => {
            if name.ends_with(".rs") {
                let rel = display_rel(abs);
                out.push((abs.clone(), rel));
            }
        }
    }
}

/// Best-effort relative path: try CWD, fall back to absolute.
fn display_rel(abs: &Path) -> String {
    let cwd = std::env::current_dir().ok();
    if let Some(cwd) = cwd {
        if let Ok(rel) = abs.strip_prefix(&cwd) {
            return rel.to_string_lossy().replace('\\', "/");
        }
    }
    abs.to_string_lossy().replace('\\', "/")
}

fn scan_file(abs: &Path, rel: &str, target: &str, mode: Mode, hits: &mut BTreeSet<Hit>) {
    let Ok(src) = fs::read_to_string(abs) else {
        return;
    };
    let Ok(file) = syn::parse_file(&src) else {
        return;
    };

    let want_defs = matches!(mode, Mode::Both | Mode::DefsOnly);
    let want_uses = matches!(mode, Mode::Both | Mode::UsesOnly);

    let mut def_locs: BTreeSet<(usize, usize)> = BTreeSet::new();

    if want_defs {
        collect_defs(&file.items, target, rel, hits, &mut def_locs);
    } else {
        // Still need def locations to suppress duplicate use hits at the
        // ident itself (e.g. `fn foo` would match `foo` as a path token
        // if a visitor hit the signature).
        collect_def_locs(&file.items, target, &mut def_locs);
    }

    if want_uses {
        let mut v = UseCollector {
            target,
            rel,
            hits,
            skip: &def_locs,
        };
        v.visit_file(&file);
    }
}

fn push_def(
    hits: &mut BTreeSet<Hit>,
    def_locs: &mut BTreeSet<(usize, usize)>,
    rel: &str,
    kind: &'static str,
    name: &str,
    ident_span: Span,
) {
    let (l, c) = (ident_span.start().line, ident_span.start().column + 1);
    def_locs.insert((l, c));
    hits.insert(Hit {
        file: rel.to_string(),
        line: l,
        col: c,
        role: "def",
        kind,
        name: name.to_string(),
    });
}

fn record_def_loc(def_locs: &mut BTreeSet<(usize, usize)>, span: Span) {
    def_locs.insert((span.start().line, span.start().column + 1));
}

fn collect_defs(
    items: &[Item],
    target: &str,
    rel: &str,
    hits: &mut BTreeSet<Hit>,
    def_locs: &mut BTreeSet<(usize, usize)>,
) {
    for item in items {
        match item {
            Item::Fn(f) if f.sig.ident == target => {
                push_def(hits, def_locs, rel, "fn", target, f.sig.ident.span());
            }
            Item::Struct(s) if s.ident == target => {
                push_def(hits, def_locs, rel, "struct", target, s.ident.span());
            }
            Item::Enum(e) if e.ident == target => {
                push_def(hits, def_locs, rel, "enum", target, e.ident.span());
            }
            Item::Union(u) if u.ident == target => {
                push_def(hits, def_locs, rel, "union", target, u.ident.span());
            }
            Item::Trait(t) if t.ident == target => {
                push_def(hits, def_locs, rel, "trait", target, t.ident.span());
            }
            Item::Const(c) if c.ident == target => {
                push_def(hits, def_locs, rel, "const", target, c.ident.span());
            }
            Item::Static(s) if s.ident == target => {
                push_def(hits, def_locs, rel, "static", target, s.ident.span());
            }
            Item::Type(t) if t.ident == target => {
                push_def(hits, def_locs, rel, "type", target, t.ident.span());
            }
            Item::Macro(m) => {
                if let Some(id) = m.ident.as_ref() {
                    if id == target {
                        push_def(hits, def_locs, rel, "macro", target, id.span());
                    }
                }
            }
            Item::Mod(m) => {
                if let Some((_, items)) = &m.content {
                    collect_defs(items, target, rel, hits, def_locs);
                }
            }
            Item::Impl(imp) => {
                collect_impl_defs(imp, target, rel, hits, def_locs);
            }
            Item::Trait(t) => {
                for ti in &t.items {
                    if let TraitItem::Fn(f) = ti {
                        if f.sig.ident == target {
                            push_def(hits, def_locs, rel, "method", target, f.sig.ident.span());
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn collect_impl_defs(
    imp: &ItemImpl,
    target: &str,
    rel: &str,
    hits: &mut BTreeSet<Hit>,
    def_locs: &mut BTreeSet<(usize, usize)>,
) {
    for ii in &imp.items {
        match ii {
            ImplItem::Fn(f) if f.sig.ident == target => {
                push_def(hits, def_locs, rel, "method", target, f.sig.ident.span());
            }
            ImplItem::Const(c) if c.ident == target => {
                push_def(hits, def_locs, rel, "const", target, c.ident.span());
            }
            ImplItem::Type(t) if t.ident == target => {
                push_def(hits, def_locs, rel, "type", target, t.ident.span());
            }
            _ => {}
        }
    }
}

/// Just record def-ident spans (no Hit emission). Used when `--uses-only`
/// to keep dedup working without polluting output with defs.
fn collect_def_locs(items: &[Item], target: &str, def_locs: &mut BTreeSet<(usize, usize)>) {
    for item in items {
        match item {
            Item::Fn(f) if f.sig.ident == target => record_def_loc(def_locs, f.sig.ident.span()),
            Item::Struct(s) if s.ident == target => record_def_loc(def_locs, s.ident.span()),
            Item::Enum(e) if e.ident == target => record_def_loc(def_locs, e.ident.span()),
            Item::Union(u) if u.ident == target => record_def_loc(def_locs, u.ident.span()),
            Item::Trait(t) if t.ident == target => record_def_loc(def_locs, t.ident.span()),
            Item::Const(c) if c.ident == target => record_def_loc(def_locs, c.ident.span()),
            Item::Static(s) if s.ident == target => record_def_loc(def_locs, s.ident.span()),
            Item::Type(t) if t.ident == target => record_def_loc(def_locs, t.ident.span()),
            Item::Macro(m) => {
                if let Some(id) = m.ident.as_ref() {
                    if id == target {
                        record_def_loc(def_locs, id.span());
                    }
                }
            }
            Item::Mod(m) => {
                if let Some((_, items)) = &m.content {
                    collect_def_locs(items, target, def_locs);
                }
            }
            Item::Impl(imp) => {
                for ii in &imp.items {
                    match ii {
                        ImplItem::Fn(f) if f.sig.ident == target => {
                            record_def_loc(def_locs, f.sig.ident.span());
                        }
                        ImplItem::Const(c) if c.ident == target => {
                            record_def_loc(def_locs, c.ident.span());
                        }
                        ImplItem::Type(t) if t.ident == target => {
                            record_def_loc(def_locs, t.ident.span());
                        }
                        _ => {}
                    }
                }
            }
            Item::Trait(t) => {
                for ti in &t.items {
                    if let TraitItem::Fn(f) = ti {
                        if f.sig.ident == target {
                            record_def_loc(def_locs, f.sig.ident.span());
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

struct UseCollector<'a> {
    target: &'a str,
    rel: &'a str,
    hits: &'a mut BTreeSet<Hit>,
    skip: &'a BTreeSet<(usize, usize)>,
}

impl<'a> UseCollector<'a> {
    fn push(&mut self, kind: &'static str, span: Span) {
        let (l, c) = (span.start().line, span.start().column + 1);
        if self.skip.contains(&(l, c)) {
            return;
        }
        self.hits.insert(Hit {
            file: self.rel.to_string(),
            line: l,
            col: c,
            role: "use",
            kind,
            name: self.target.to_string(),
        });
    }

    fn last_seg_matches(&self, p: &syn::Path) -> Option<Span> {
        p.segments
            .last()
            .filter(|s| s.ident == self.target)
            .map(|s| s.ident.span())
    }
}

impl<'ast, 'a> Visit<'ast> for UseCollector<'a> {
    fn visit_expr_call(&mut self, c: &'ast ExprCall) {
        if let Expr::Path(p) = &*c.func {
            if let Some(span) = self.last_seg_matches(&p.path) {
                self.push("call", span);
            }
            // Skip recursing into the func path so visit_expr_path doesn't
            // double-fire as a "path" use on the same ident.
            for arg in &c.args {
                self.visit_expr(arg);
            }
            return;
        }
        visit::visit_expr_call(self, c);
    }

    fn visit_expr_method_call(&mut self, m: &'ast ExprMethodCall) {
        if m.method == self.target {
            self.push("method", m.method.span());
        }
        visit::visit_expr_method_call(self, m);
    }

    fn visit_expr_struct(&mut self, s: &'ast ExprStruct) {
        if let Some(span) = self.last_seg_matches(&s.path) {
            self.push("struct-lit", span);
        }
        for f in &s.fields {
            self.visit_expr(&f.expr);
        }
        if let Some(rest) = &s.rest {
            self.visit_expr(rest);
        }
    }

    fn visit_expr_path(&mut self, e: &'ast ExprPath) {
        if let Some(span) = self.last_seg_matches(&e.path) {
            self.push("path", span);
        }
        visit::visit_expr_path(self, e);
    }

    fn visit_type_path(&mut self, t: &'ast TypePath) {
        if let Some(span) = self.last_seg_matches(&t.path) {
            self.push("type", span);
        }
        visit::visit_type_path(self, t);
    }

    fn visit_macro(&mut self, m: &'ast Macro) {
        if let Some(span) = self.last_seg_matches(&m.path) {
            self.push("macro", span);
        }
        // Don't descend into macro tokens: not parsed.
    }

    fn visit_use_path(&mut self, u: &'ast UsePath) {
        if u.ident == self.target {
            self.push("import", u.ident.span());
        }
        visit::visit_use_path(self, u);
    }

    fn visit_use_name(&mut self, u: &'ast UseName) {
        if u.ident == self.target {
            self.push("import", u.ident.span());
        }
    }

    fn visit_use_rename(&mut self, u: &'ast UseRename) {
        if u.ident == self.target {
            self.push("import", u.ident.span());
        }
    }

    fn visit_pat(&mut self, p: &'ast Pat) {
        match p {
            Pat::Path(PatPath { path, .. }) => {
                if let Some(span) = self.last_seg_matches(path) {
                    self.push("pat", span);
                }
            }
            Pat::Struct(PatStruct { path, fields, .. }) => {
                if let Some(span) = self.last_seg_matches(path) {
                    self.push("pat", span);
                }
                for f in fields {
                    self.visit_pat(&f.pat);
                }
                return;
            }
            Pat::TupleStruct(PatTupleStruct { path, elems, .. }) => {
                if let Some(span) = self.last_seg_matches(path) {
                    self.push("pat", span);
                }
                for e in elems {
                    self.visit_pat(e);
                }
                return;
            }
            _ => {}
        }
        visit::visit_pat(self, p);
    }

    fn visit_item_impl(&mut self, imp: &'ast ItemImpl) {
        // Self type may name the target (e.g. `impl Foo { ... }`).
        if let Type::Path(tp) = &*imp.self_ty {
            if let Some(span) = self.last_seg_matches(&tp.path) {
                self.push("type", span);
            }
        }
        if let Some((_, path, _)) = &imp.trait_ {
            if let Some(span) = self.last_seg_matches(path) {
                self.push("type", span);
            }
        }
        for ii in &imp.items {
            self.visit_impl_item(ii);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(src: &str, target: &str, mode: Mode) -> Vec<Hit> {
        let file = syn::parse_file(src).unwrap();
        let mut hits = BTreeSet::new();
        let mut def_locs = BTreeSet::new();
        let want_defs = matches!(mode, Mode::Both | Mode::DefsOnly);
        let want_uses = matches!(mode, Mode::Both | Mode::UsesOnly);
        if want_defs {
            collect_defs(&file.items, target, "x.rs", &mut hits, &mut def_locs);
        } else {
            collect_def_locs(&file.items, target, &mut def_locs);
        }
        if want_uses {
            let mut v = UseCollector {
                target,
                rel: "x.rs",
                hits: &mut hits,
                skip: &def_locs,
            };
            v.visit_file(&file);
        }
        hits.into_iter().collect()
    }

    #[test]
    fn finds_fn_def_and_call() {
        let src = "fn foo() {} fn bar() { foo(); }";
        let h = scan(src, "foo", Mode::Both);
        let kinds: Vec<_> = h.iter().map(|x| (x.role, x.kind)).collect();
        assert!(kinds.contains(&("def", "fn")));
        assert!(kinds.contains(&("use", "call")));
    }

    #[test]
    fn finds_method_call_and_def() {
        let src = "struct S; impl S { fn ping(&self) {} } fn x(s: S) { s.ping(); }";
        let h = scan(src, "ping", Mode::Both);
        let kinds: Vec<_> = h.iter().map(|x| (x.role, x.kind)).collect();
        assert!(kinds.contains(&("def", "method")));
        assert!(kinds.contains(&("use", "method")));
    }

    #[test]
    fn finds_struct_def_lit_type_and_pat() {
        let src = r#"
            struct Foo { x: u32 }
            fn a() -> Foo { Foo { x: 1 } }
            fn b(f: Foo) { let Foo { x } = f; let _ = x; }
        "#;
        let h = scan(src, "Foo", Mode::Both);
        let kinds: Vec<_> = h.iter().map(|x| x.kind).collect();
        assert!(kinds.contains(&"struct"));
        assert!(kinds.contains(&"struct-lit"));
        assert!(kinds.contains(&"type"));
        assert!(kinds.contains(&"pat"));
    }

    #[test]
    fn finds_use_import() {
        let src = "use crate::foo::bar; fn x() { bar(); }";
        let h = scan(src, "bar", Mode::Both);
        let kinds: Vec<_> = h.iter().map(|x| x.kind).collect();
        assert!(kinds.contains(&"import"));
        assert!(kinds.contains(&"call"));
    }

    #[test]
    fn finds_macro_invocation() {
        let src = r#"fn x() { println!("hi"); }"#;
        let h = scan(src, "println", Mode::Both);
        assert!(h.iter().any(|x| x.kind == "macro" && x.role == "use"));
    }

    #[test]
    fn defs_only_excludes_uses() {
        let src = "fn foo() {} fn bar() { foo(); }";
        let h = scan(src, "foo", Mode::DefsOnly);
        assert!(h.iter().all(|x| x.role == "def"));
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn uses_only_excludes_defs_and_does_not_double_count_def_ident() {
        let src = "fn foo() {} fn bar() { foo(); foo(); }";
        let h = scan(src, "foo", Mode::UsesOnly);
        assert!(h.iter().all(|x| x.role == "use"));
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn no_hits_returns_empty() {
        let src = "fn foo() {}";
        let h = scan(src, "nonexistent", Mode::Both);
        assert!(h.is_empty());
    }

    #[test]
    fn matches_last_path_segment_only() {
        let src = "use a::b::Target; fn x() -> a::b::Target { unimplemented!() }";
        let h = scan(src, "Target", Mode::Both);
        assert!(h.iter().any(|x| x.kind == "import"));
        assert!(h.iter().any(|x| x.kind == "type"));
    }
}
