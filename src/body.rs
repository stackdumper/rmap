//! Print the full source body of a Rust item by name.
//!
//! Resolves a symbol to its definition span via `syn`, then slices the
//! file by line range. Eliminates the `rmap module --lines` → `Read
//! offset/limit` two-step that agents otherwise rely on after locating a
//! function with `refs`.
//!
//! Name forms:
//!   - `foo`           — match by trailing ident (any kind)
//!   - `Foo::bar`      — impl method `bar` on type `Foo`
//!
//! Disambiguation: `--in PATH` scopes; `--kind KIND` filters. If still
//! ambiguous, all matches are printed with headers, blank line between.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use proc_macro2::Span;
use syn::spanned::Spanned;
use syn::{ImplItem, Item, ItemImpl, TraitItem, Type};

use crate::refs;
use crate::walk::{self, Filter};

pub struct BodyOptions {
    /// Symbol to print. Accepts `name` or `Type::method`.
    pub name: String,
    /// Filter by item kind: fn, method, struct, enum, union, trait, impl,
    /// const, static, type, macro. None = any.
    pub kind: Option<String>,
}

struct Match {
    file: String,
    kind: &'static str,
    /// Display name (e.g. `Foo::bar` for impl methods, `Debug for Foo` for
    /// trait-impl blocks).
    name: String,
    start: usize,
    end: usize,
}

/// Split `Type::name` into (`Some("Type")`, `"name"`). Returns
/// `(None, whole)` if no `::` separator or either side is empty.
fn split_name(s: &str) -> (Option<&str>, &str) {
    match s.rsplit_once("::") {
        Some((t, n)) if !t.is_empty() && !n.is_empty() => (Some(t), n),
        _ => (None, s),
    }
}

pub fn run(roots: &[PathBuf], opts: &BodyOptions) -> String {
    let (type_filter, target) = split_name(&opts.name);

    let mut files: Vec<(PathBuf, String)> = Vec::new();
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    for root in roots {
        if root.is_file() {
            if root.extension().and_then(|e| e.to_str()) == Some("rs") {
                let abs = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
                if seen.insert(abs.clone()) {
                    let rel = refs::display_rel(&abs);
                    files.push((abs, rel));
                }
            }
        } else {
            let filter = Filter {
                exclude: Vec::new(),
                ext: vec!["rs".to_string()],
            };
            match walk::enumerate(root, &filter) {
                Ok(tree) => {
                    let mut tmp = Vec::new();
                    refs::collect_rs_files(&tree, &mut tmp);
                    for (abs, rel) in tmp {
                        if seen.insert(abs.clone()) {
                            files.push((abs, rel));
                        }
                    }
                }
                Err(e) => return format!("{e}\n"),
            }
        }
    }

    let mut matches: Vec<Match> = Vec::new();
    for (abs, rel) in &files {
        scan_file(abs, rel, target, type_filter, &mut matches);
    }

    if let Some(k) = &opts.kind {
        matches.retain(|m| m.kind == k);
    }

    if matches.is_empty() {
        let mut idents: BTreeSet<String> = BTreeSet::new();
        for (abs, _) in &files {
            if let Ok(src) = fs::read_to_string(abs) {
                refs::collect_idents(&src, &mut idents);
            }
        }
        let sugg = refs::suggest(target, &idents, 5);
        if sugg.is_empty() {
            return format!("no symbol `{}` found\n", opts.name);
        }
        return format!(
            "no symbol `{}` found\ndid you mean: {}?\n",
            opts.name,
            sugg.join(", ")
        );
    }

    let mut src_cache: HashMap<String, Vec<String>> = HashMap::new();
    let abs_by_rel: HashMap<&str, &PathBuf> =
        files.iter().map(|(abs, rel)| (rel.as_str(), abs)).collect();

    let mut out = String::new();
    for (i, m) in matches.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&format!(
            "// {}:{}-{} {} {}\n",
            m.file, m.start, m.end, m.kind, m.name
        ));
        let lines = src_cache.entry(m.file.clone()).or_insert_with(|| {
            abs_by_rel
                .get(m.file.as_str())
                .and_then(|p| fs::read_to_string(p).ok())
                .map(|s| s.lines().map(str::to_string).collect())
                .unwrap_or_default()
        });
        let total = lines.len();
        let s = m.start.max(1);
        let e = m.end.min(total);
        for n in s..=e {
            let content = lines.get(n - 1).map(String::as_str).unwrap_or("");
            out.push_str(content);
            out.push('\n');
        }
    }
    out
}

fn scan_file(abs: &Path, rel: &str, target: &str, type_filter: Option<&str>, out: &mut Vec<Match>) {
    let Ok(src) = fs::read_to_string(abs) else {
        return;
    };
    scan_src(&src, rel, target, type_filter, out);
}

fn scan_src(src: &str, rel: &str, target: &str, type_filter: Option<&str>, out: &mut Vec<Match>) {
    let Ok(file) = syn::parse_file(src) else {
        return;
    };
    collect(&file.items, target, type_filter, rel, out);
}

fn collect(
    items: &[Item],
    target: &str,
    type_filter: Option<&str>,
    rel: &str,
    out: &mut Vec<Match>,
) {
    let owns_type_filter = type_filter.is_some();
    for item in items {
        match item {
            Item::Mod(m) => {
                if let Some((_, items)) = &m.content {
                    collect(items, target, type_filter, rel, out);
                }
            }
            Item::Impl(imp) => collect_impl(imp, target, type_filter, rel, out),
            Item::Trait(t) => collect_trait(t, target, type_filter, rel, out),
            // Free-standing item arms are only candidates when the user
            // did NOT scope with `Type::name`.
            _ if owns_type_filter => {}
            Item::Fn(f) if f.sig.ident == target => {
                push(out, rel, "fn", target.into(), f.span());
            }
            Item::Struct(s) if s.ident == target => {
                push(out, rel, "struct", target.into(), s.span());
            }
            Item::Enum(e) if e.ident == target => {
                push(out, rel, "enum", target.into(), e.span());
            }
            Item::Union(u) if u.ident == target => {
                push(out, rel, "union", target.into(), u.span());
            }
            Item::Const(c) if c.ident == target => {
                push(out, rel, "const", target.into(), c.span());
            }
            Item::Static(s) if s.ident == target => {
                push(out, rel, "static", target.into(), s.span());
            }
            Item::Type(t) if t.ident == target => {
                push(out, rel, "type", target.into(), t.span());
            }
            Item::Macro(m) => {
                if let Some(id) = m.ident.as_ref() {
                    if id == target {
                        push(out, rel, "macro", target.into(), m.span());
                    }
                }
            }
            _ => {}
        }
    }
}

fn collect_trait(
    t: &syn::ItemTrait,
    target: &str,
    type_filter: Option<&str>,
    rel: &str,
    out: &mut Vec<Match>,
) {
    // Whole-trait body (only when not scoped by `Type::name`).
    if type_filter.is_none() && t.ident == target {
        push(out, rel, "trait", target.into(), t.span());
    }
    // Default-impl method bodies. Honour `Type::name` scope.
    if type_filter.map(|tf| t.ident == tf).unwrap_or(true) {
        for ti in &t.items {
            if let TraitItem::Fn(f) = ti {
                if f.sig.ident == target {
                    let display = format!("{}::{}", t.ident, target);
                    push(out, rel, "method", display, f.span());
                }
            }
        }
    }
}

fn collect_impl(
    imp: &ItemImpl,
    target: &str,
    type_filter: Option<&str>,
    rel: &str,
    out: &mut Vec<Match>,
) {
    let self_name = self_type_name(&imp.self_ty);

    // Whole-impl-block match (`rmap body Foo --kind impl`, or
    // `rmap body 'Debug for Foo' --kind impl`). Skipped when the user
    // scoped with `Type::name`, which only addresses items inside.
    if type_filter.is_none() {
        if let Some(n) = &self_name {
            match &imp.trait_ {
                None if n == target => {
                    push(out, rel, "impl", n.clone(), imp.span());
                }
                Some((_, path, _)) => {
                    let trait_name = path
                        .segments
                        .last()
                        .map(|s| s.ident.to_string())
                        .unwrap_or_default();
                    let display = format!("{trait_name} for {n}");
                    if trait_name == target || display == target {
                        push(out, rel, "impl", display, imp.span());
                    }
                }
                _ => {}
            }
        }
    }

    // When scoped, the impl's self type must match.
    if let Some(tf) = type_filter {
        if self_name.as_deref() != Some(tf) {
            return;
        }
    }

    for ii in &imp.items {
        let (kind, span) = match ii {
            ImplItem::Fn(f) if f.sig.ident == target => ("method", f.span()),
            ImplItem::Const(c) if c.ident == target => ("const", c.span()),
            ImplItem::Type(t) if t.ident == target => ("type", t.span()),
            _ => continue,
        };
        let display = match &self_name {
            Some(n) => format!("{n}::{target}"),
            None => target.to_string(),
        };
        push(out, rel, kind, display, span);
    }
}

fn push(out: &mut Vec<Match>, rel: &str, kind: &'static str, name: String, span: Span) {
    out.push(Match {
        file: rel.to_string(),
        kind,
        name,
        start: span.start().line,
        end: span.end().line,
    });
}

fn self_type_name(ty: &Type) -> Option<String> {
    if let Type::Path(tp) = ty {
        tp.path.segments.last().map(|s| s.ident.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(src: &str, target: &str, kind: Option<&str>) -> Vec<Match> {
        let (type_filter, name) = split_name(target);
        let mut out = Vec::new();
        scan_src(src, "x.rs", name, type_filter, &mut out);
        if let Some(k) = kind {
            out.retain(|m| m.kind == k);
        }
        out
    }

    #[test]
    fn finds_fn() {
        let src = "pub fn foo() -> u32 {\n    42\n}\n";
        let ms = find(src, "foo", None);
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].kind, "fn");
        assert_eq!(ms[0].start, 1);
        assert_eq!(ms[0].end, 3);
    }

    #[test]
    fn impl_method_via_double_colon() {
        let src = "\
pub struct Foo;
impl Foo {
    pub fn bar(&self) -> u32 { 1 }
    pub fn baz(&self) -> u32 { 2 }
}
";
        let ms = find(src, "Foo::bar", None);
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].kind, "method");
        assert_eq!(ms[0].name, "Foo::bar");
    }

    #[test]
    fn struct_span_includes_braces() {
        let src = "pub struct S {\n    pub a: u32,\n}\n";
        let ms = find(src, "S", None);
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].kind, "struct");
        assert_eq!(ms[0].start, 1);
        assert_eq!(ms[0].end, 3);
    }

    #[test]
    fn kind_filter() {
        let src = "pub fn x() {}\npub struct X;\n";
        let ms = find(src, "x", Some("fn"));
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].kind, "fn");
    }

    #[test]
    fn impl_block_by_type_name() {
        let src = "pub struct Foo;\nimpl Foo {\n    fn a(&self) {}\n}\n";
        let ms = find(src, "Foo", Some("impl"));
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].kind, "impl");
        assert_eq!(ms[0].name, "Foo");
        assert_eq!(ms[0].start, 2);
        assert_eq!(ms[0].end, 4);
    }

    #[test]
    fn nested_mod() {
        let src = "mod inner {\n    pub fn deep() {}\n}\n";
        let ms = find(src, "deep", None);
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].kind, "fn");
        assert_eq!(ms[0].start, 2);
    }
}
