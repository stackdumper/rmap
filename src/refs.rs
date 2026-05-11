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

use proc_macro2::{Span, TokenStream, TokenTree};
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
    /// When `Some(N)`, print `±N` lines of source context around each hit.
    pub excerpt: Option<usize>,
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
        match walk::enumerate(root, &filter) {
            Ok(tree) => collect_rs_files(&tree, &mut files),
            Err(e) => return format!("{e}\n"),
        }
    }

    let mut hits: BTreeSet<Hit> = BTreeSet::new();
    for (abs, rel) in &files {
        scan_file(abs, rel, &opts.name, opts.mode, &mut hits);
    }

    if hits.is_empty() {
        let mut idents: BTreeSet<String> = BTreeSet::new();
        for (abs, _) in &files {
            if let Ok(src) = fs::read_to_string(abs) {
                collect_idents(&src, &mut idents);
            }
        }
        let sugg = suggest(&opts.name, &idents, 5);
        if sugg.is_empty() {
            return format!("no hits for `{}`\n", opts.name);
        }
        return format!(
            "no hits for `{}`\ndid you mean: {}?\n",
            opts.name,
            sugg.join(", ")
        );
    }
    let mut out = String::new();
    let mut src_cache: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for h in &hits {
        out.push_str(&format!(
            "{}:{}:{} {} {} {}\n",
            h.file, h.line, h.col, h.role, h.kind, h.name
        ));
        if let Some(ctx) = opts.excerpt {
            let lines = src_cache.entry(h.file.clone()).or_insert_with(|| {
                let abs = files
                    .iter()
                    .find(|(_, rel)| rel == &h.file)
                    .map(|(abs, _)| abs.clone());
                abs.and_then(|p| fs::read_to_string(p).ok())
                    .map(|s| s.lines().map(str::to_string).collect())
                    .unwrap_or_default()
            });
            render_excerpt(&mut out, lines, h.line, ctx);
        }
    }
    out
}

/// Append `±ctx` source lines around `hit_line` (1-indexed) to `out`.
/// Hit line prefixed with `>`, others with ` `. Line numbers right-padded
/// to width of the largest emitted line number.
fn render_excerpt(out: &mut String, lines: &[String], hit_line: usize, ctx: usize) {
    if lines.is_empty() {
        return;
    }
    let total = lines.len();
    let start = hit_line.saturating_sub(ctx).max(1);
    let end = (hit_line + ctx).min(total);
    let width = end.to_string().len();
    for n in start..=end {
        let marker = if n == hit_line { '>' } else { ' ' };
        // `lines` is 0-indexed; user-facing `n` is 1-indexed.
        let content = lines.get(n - 1).map(String::as_str).unwrap_or("");
        out.push_str(&format!(
            "  {marker} {n:>width$} | {content}\n",
            marker = marker,
            n = n,
            width = width,
            content = content,
        ));
    }
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

/// Best-effort relative path: stripped against the process CWD when
/// possible (so `rmap refs Foo --in src/sub` from the repo root prints
/// `src/sub/file.rs:...`). Falls back to the absolute path if `abs` is
/// outside CWD or CWD is unavailable.
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

/// Pull every `Ident` from a Rust source file via a `proc_macro2`
/// token-tree walk. Keywords (e.g. `fn`, `let`) leak in too, but they're
/// filtered later by the similarity threshold against the user's query.
fn collect_idents(src: &str, out: &mut BTreeSet<String>) {
    if let Ok(ts) = src.parse::<TokenStream>() {
        walk_tokens(ts, out);
    }
}

fn walk_tokens(ts: TokenStream, out: &mut BTreeSet<String>) {
    for t in ts {
        match t {
            TokenTree::Ident(i) => {
                out.insert(i.to_string());
            }
            TokenTree::Group(g) => walk_tokens(g.stream(), out),
            _ => {}
        }
    }
}

/// Split an identifier into lowercased tokens on `_` and camelCase
/// boundaries. `render_HUDBar` -> ["render", "hud", "bar"].
fn tokenize(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in s.split('_') {
        if part.is_empty() {
            continue;
        }
        let mut cur = String::new();
        let chars: Vec<char> = part.chars().collect();
        for i in 0..chars.len() {
            let c = chars[i];
            let prev = if i > 0 { Some(chars[i - 1]) } else { None };
            let next = if i + 1 < chars.len() {
                Some(chars[i + 1])
            } else {
                None
            };
            let boundary = match (prev, c, next) {
                (Some(p), c, _) if p.is_lowercase() && c.is_uppercase() => true,
                (Some(p), c, Some(n))
                    if p.is_uppercase() && c.is_uppercase() && n.is_lowercase() =>
                {
                    true
                }
                _ => false,
            };
            if boundary && !cur.is_empty() {
                out.push(cur.to_lowercase());
                cur = String::new();
            }
            cur.push(c);
        }
        if !cur.is_empty() {
            out.push(cur.to_lowercase());
        }
    }
    out
}

fn levenshtein(a: &str, b: &str) -> usize {
    let av: Vec<char> = a.chars().collect();
    let bv: Vec<char> = b.chars().collect();
    let (n, m) = (av.len(), bv.len());
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur: Vec<usize> = vec![0; m + 1];
    for i in 1..=n {
        cur[0] = i;
        for j in 1..=m {
            let cost = if av[i - 1] == bv[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1)
                .min(cur[j - 1] + 1)
                .min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[m]
}

fn bigrams(s: &str) -> BTreeSet<(char, char)> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = BTreeSet::new();
    for w in chars.windows(2) {
        out.insert((w[0], w[1]));
    }
    out
}

fn jaccard(a: &BTreeSet<(char, char)>, b: &BTreeSet<(char, char)>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    inter / union
}

fn common_prefix(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}

/// Score how well `cand` matches `query`. Returns 0 when no real signal
/// (gate); otherwise a composite of substring, token overlap, bigram
/// Jaccard, longest common prefix, and edit distance.
fn score(query: &str, cand: &str) -> i32 {
    let ql = query.to_lowercase();
    let cl = cand.to_lowercase();
    if ql == cl {
        return 10_000;
    }

    let qb = bigrams(&ql);
    let cb = bigrams(&cl);
    let jacc = jaccard(&qb, &cb);

    let qt = tokenize(query);
    let ct = tokenize(cand);
    let shared_tok = qt.iter().filter(|t| ct.contains(t)).count();

    let lcp = common_prefix(&ql, &cl);
    let lev = levenshtein(&ql, &cl);
    let maxlen = ql.len().max(cl.len());

    let substring_hit =
        ql.len() >= 3 && cl.len() >= 3 && (cl.contains(&ql) || ql.contains(&cl));
    let close_typo = maxlen >= 4 && (lev as f64) / (maxlen as f64) <= 0.34;

    // Gate: require a meaningful match signal. Filters out short-lev
    // noise like `Bar` vs `baner`.
    let pass = substring_hit
        || shared_tok > 0
        || jacc >= 0.5
        || (close_typo && lcp >= 2);
    if !pass {
        return 0;
    }

    let mut s: i32 = 0;
    if substring_hit {
        if cl.contains(&ql) {
            s += 200;
        }
        if ql.contains(&cl) {
            s += 100;
        }
    }
    s += shared_tok as i32 * 100;
    s += (jacc * 200.0) as i32;
    s += lcp as i32 * 25;
    if maxlen > 0 {
        s += ((maxlen - lev) as i32) * 4;
    }
    if maxlen >= 4 && lev <= 2 {
        s += 60;
    }
    s
}

/// Rank `idents` by similarity to `query`. Returns up to `n` candidates
/// above an empirical floor; empty if nothing crosses the bar.
fn suggest(query: &str, idents: &BTreeSet<String>, n: usize) -> Vec<String> {
    const FLOOR: i32 = 60;
    let mut scored: Vec<(i32, &String)> = idents
        .iter()
        .filter(|i| *i != query && i.len() > 1)
        .map(|i| (score(query, i), i))
        .filter(|(s, _)| *s >= FLOOR)
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    scored.into_iter().take(n).map(|(_, s)| s.clone()).collect()
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
    fn tokenize_splits_snake_and_camel() {
        assert_eq!(tokenize("render_HUDBar"), vec!["render", "hud", "bar"]);
        assert_eq!(tokenize("draw_banner"), vec!["draw", "banner"]);
        assert_eq!(tokenize("MyXMLParser"), vec!["my", "xml", "parser"]);
    }

    #[test]
    fn suggest_finds_substring_token() {
        let mut s: BTreeSet<String> = BTreeSet::new();
        for x in ["render_banner", "draw_hud", "status_bar", "totally_unrelated"] {
            s.insert(x.to_string());
        }
        let out = suggest("banner", &s, 5);
        assert!(out.contains(&"render_banner".to_string()));
        assert_eq!(out[0], "render_banner");
    }

    #[test]
    fn suggest_levenshtein_close() {
        let mut s: BTreeSet<String> = BTreeSet::new();
        for x in ["bannar", "banister", "x"] {
            s.insert(x.to_string());
        }
        let out = suggest("banner", &s, 5);
        assert!(out.contains(&"bannar".to_string()));
    }

    #[test]
    fn suggest_floor_excludes_garbage() {
        let mut s: BTreeSet<String> = BTreeSet::new();
        for x in ["xyz", "qqq", "zzzzzz"] {
            s.insert(x.to_string());
        }
        let out = suggest("banner", &s, 5);
        assert!(out.is_empty());
    }

    #[test]
    fn suggest_gates_short_lev_noise() {
        // `Bar`/`Lander`/`Namer` are short-edit-distance to `baner` but
        // semantically unrelated. Should be filtered out by the gate.
        let mut s: BTreeSet<String> = BTreeSet::new();
        for x in ["Bar", "Lander", "Namer", "banner", "banners"] {
            s.insert(x.to_string());
        }
        let out = suggest("baner", &s, 5);
        assert!(!out.contains(&"Bar".to_string()));
        assert!(!out.contains(&"Lander".to_string()));
        assert!(!out.contains(&"Namer".to_string()));
        assert!(out.contains(&"banner".to_string()));
        // Real typo target ranks above other matches.
        assert_eq!(out[0], "banner");
    }

    #[test]
    fn suggest_prefix_boost() {
        let mut s: BTreeSet<String> = BTreeSet::new();
        for x in ["banner_h", "BANNER_CAP", "unrelated_banner_thing"] {
            s.insert(x.to_string());
        }
        let out = suggest("banner", &s, 5);
        // Prefix matches should beat mid-string matches.
        let idx_prefix = out.iter().position(|x| x == "banner_h");
        let idx_mid = out.iter().position(|x| x == "unrelated_banner_thing");
        assert!(idx_prefix.is_some());
        if let (Some(p), Some(m)) = (idx_prefix, idx_mid) {
            assert!(p < m, "prefix={p}, mid={m}, out={out:?}");
        }
    }

    #[test]
    fn matches_last_path_segment_only() {
        let src = "use a::b::Target; fn x() -> a::b::Target { unimplemented!() }";
        let h = scan(src, "Target", Mode::Both);
        assert!(h.iter().any(|x| x.kind == "import"));
        assert!(h.iter().any(|x| x.kind == "type"));
    }
}
