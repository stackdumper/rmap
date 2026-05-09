//! Rust item extraction. Given a `.rs` file path, return a brace-formatted
//! summary of the items it defines: structs, enums, traits, impls, fns,
//! consts, statics, type aliases, and macros.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::Path;

use proc_macro2::Span;
use syn::spanned::Spanned;
use syn::{ImplItem, Item, ItemImpl, TraitItem, Type};

#[derive(Clone, Copy, Default)]
pub struct ParseOptions {
    /// Annotate every parsed symbol with `:start-end` source line ranges.
    pub lines: bool,
}

/// Returns the inline item summary for one `.rs` file, or `None` on parse
/// failure or empty content.
pub fn render_file(path: &Path, options: ParseOptions) -> Option<String> {
    let src = fs::read_to_string(path).ok()?;
    render_str(&src, options)
}

/// Render items from a Rust source string. Exposed for tests.
pub fn render_str(src: &str, options: ParseOptions) -> Option<String> {
    let syntax = syn::parse_file(src).ok()?;
    render_items(&syntax, options)
}

fn render_items(syntax: &syn::File, options: ParseOptions) -> Option<String> {
    let mut local_types: HashSet<String> = HashSet::new();
    for item in &syntax.items {
        match item {
            Item::Struct(s) => {
                local_types.insert(s.ident.to_string());
            }
            Item::Enum(e) => {
                local_types.insert(e.ident.to_string());
            }
            Item::Union(u) => {
                local_types.insert(u.ident.to_string());
            }
            Item::Trait(t) => {
                local_types.insert(t.ident.to_string());
            }
            _ => {}
        }
    }

    let mut local_methods: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for item in &syntax.items {
        if let Item::Impl(imp) = item {
            if imp.trait_.is_none() {
                if let Some(name) = self_type_name(&imp.self_ty) {
                    if local_types.contains(&name) {
                        local_methods
                            .entry(name)
                            .or_default()
                            .extend(impl_members(imp, options));
                    }
                }
            }
        }
    }

    let mut entries: Vec<String> = Vec::new();
    for item in &syntax.items {
        match item {
            Item::Struct(s) => {
                entries.push(fmt_type(
                    "struct",
                    &s.ident.to_string(),
                    s.span(),
                    options,
                    &mut local_methods,
                ));
            }
            Item::Enum(e) => {
                entries.push(fmt_type(
                    "enum",
                    &e.ident.to_string(),
                    e.span(),
                    options,
                    &mut local_methods,
                ));
            }
            Item::Union(u) => {
                entries.push(fmt_type(
                    "union",
                    &u.ident.to_string(),
                    u.span(),
                    options,
                    &mut local_methods,
                ));
            }
            Item::Trait(t) => {
                let name = t.ident.to_string();
                let header = format!("trait {name}{}", range(t.span(), options));
                let items: Vec<String> = t
                    .items
                    .iter()
                    .filter_map(|ti| match ti {
                        TraitItem::Fn(f) => {
                            Some(fmt_named("fn", &f.sig.ident.to_string(), f.span(), options))
                        }
                        TraitItem::Type(ty) => {
                            Some(fmt_named("type", &ty.ident.to_string(), ty.span(), options))
                        }
                        TraitItem::Const(c) => {
                            Some(fmt_named("const", &c.ident.to_string(), c.span(), options))
                        }
                        _ => None,
                    })
                    .collect();
                let merged = local_methods.remove(&name).unwrap_or_default();
                let mut all = items;
                all.extend(merged);
                if all.is_empty() {
                    entries.push(header);
                } else {
                    entries.push(format!("{header} {{ {} }}", all.join(", ")));
                }
            }
            Item::Fn(f) => {
                entries.push(fmt_named("fn", &f.sig.ident.to_string(), f.span(), options));
            }
            Item::Type(t) => {
                entries.push(fmt_named("type", &t.ident.to_string(), t.span(), options));
            }
            Item::Const(c) => {
                entries.push(fmt_named("const", &c.ident.to_string(), c.span(), options));
            }
            Item::Static(s) => {
                entries.push(fmt_named("static", &s.ident.to_string(), s.span(), options));
            }
            Item::Macro(m) => {
                if let Some(ident) = m.ident.as_ref() {
                    entries.push(fmt_named("macro", &ident.to_string(), m.span(), options));
                }
            }
            Item::Impl(imp) => {
                let Some(self_name) = self_type_name(&imp.self_ty) else {
                    continue;
                };
                let r = range(imp.span(), options);
                if imp.trait_.is_none() {
                    if local_types.contains(&self_name) {
                        continue;
                    }
                    let methods = impl_members(imp, options);
                    if methods.is_empty() {
                        entries.push(format!("impl {self_name}{r}"));
                    } else {
                        entries.push(format!("impl {self_name}{r} {{ {} }}", methods.join(", ")));
                    }
                } else {
                    let trait_name = imp
                        .trait_
                        .as_ref()
                        .map(|(_, path, _)| path_tail(path))
                        .unwrap_or_default();
                    let methods = impl_members(imp, options);
                    if methods.is_empty() {
                        entries.push(format!("impl {trait_name} for {self_name}{r}"));
                    } else {
                        entries.push(format!(
                            "impl {trait_name} for {self_name}{r} {{ {} }}",
                            methods.join(", ")
                        ));
                    }
                }
            }
            _ => {}
        }
    }

    if entries.is_empty() {
        None
    } else {
        Some(entries.join(", "))
    }
}

fn fmt_type(
    kind: &str,
    name: &str,
    span: Span,
    options: ParseOptions,
    methods: &mut BTreeMap<String, Vec<String>>,
) -> String {
    let header = format!("{kind} {name}{}", range(span, options));
    let m = methods.remove(name).unwrap_or_default();
    if m.is_empty() {
        header
    } else {
        format!("{header} {{ {} }}", m.join(", "))
    }
}

fn impl_members(imp: &ItemImpl, options: ParseOptions) -> Vec<String> {
    imp.items
        .iter()
        .filter_map(|i| match i {
            ImplItem::Fn(f) => Some(fmt_named("fn", &f.sig.ident.to_string(), f.span(), options)),
            ImplItem::Type(t) => Some(fmt_named("type", &t.ident.to_string(), t.span(), options)),
            ImplItem::Const(c) => Some(fmt_named("const", &c.ident.to_string(), c.span(), options)),
            _ => None,
        })
        .collect()
}

fn fmt_named(kind: &str, name: &str, span: Span, options: ParseOptions) -> String {
    format!("{kind} {name}{}", range(span, options))
}

fn range(span: Span, options: ParseOptions) -> String {
    if !options.lines {
        return String::new();
    }
    let start = span.start().line;
    let end = span.end().line;
    format!(":{start}-{end}")
}

fn self_type_name(ty: &Type) -> Option<String> {
    if let Type::Path(tp) = ty {
        tp.path.segments.last().map(|s| s.ident.to_string())
    } else {
        None
    }
}

fn path_tail(p: &syn::Path) -> String {
    p.segments
        .last()
        .map(|s| s.ident.to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(src: &str) -> String {
        render_str(src, ParseOptions::default()).unwrap_or_default()
    }

    #[test]
    fn struct_with_inherent_methods_merges_into_braces() {
        let src = r#"
            pub struct Foo;
            impl Foo {
                pub fn new() -> Self { Foo }
                pub fn run(&self) {}
            }
        "#;
        assert_eq!(render(src), "struct Foo { fn new, fn run }");
    }

    #[test]
    fn enum_and_const_emit() {
        let src = "pub enum E { A, B } pub const N: u32 = 1;";
        assert_eq!(render(src), "enum E, const N");
    }

    #[test]
    fn trait_impl_kept_separate_from_inherent_impl() {
        let src = r#"
            pub struct Foo;
            impl Foo { pub fn a(&self) {} }
            impl std::fmt::Debug for Foo {
                fn fmt(&self, _: &mut std::fmt::Formatter) -> std::fmt::Result { Ok(()) }
            }
        "#;
        assert_eq!(
            render(src),
            "struct Foo { fn a }, impl Debug for Foo { fn fmt }"
        );
    }

    #[test]
    fn lines_annotates_every_symbol() {
        let src = "\
pub struct S;
pub enum E { A }
pub const N: u32 = 1;
pub fn f() {}
impl S { pub fn m(&self) {} }
";
        let opts = ParseOptions { lines: true };
        let out = render_str(src, opts).unwrap();
        assert!(out.contains("struct S:1-1"), "{out}");
        assert!(out.contains("enum E:2-2"), "{out}");
        assert!(out.contains("const N:3-3"), "{out}");
        assert!(out.contains("fn f:4-4"), "{out}");
        assert!(out.contains("fn m:5-5"), "{out}");
    }

    #[test]
    fn lines_annotates_trait_and_impl_headers() {
        let src = "\
pub trait T { fn a(&self); }
impl std::fmt::Debug for u32 {
    fn fmt(&self, _: &mut std::fmt::Formatter) -> std::fmt::Result { Ok(()) }
}
";
        let opts = ParseOptions { lines: true };
        let out = render_str(src, opts).unwrap();
        assert!(out.contains("trait T:1-1"), "{out}");
        assert!(out.contains("impl Debug for u32:2-4"), "{out}");
    }

    #[test]
    fn invalid_rust_returns_none() {
        assert!(render_str("not rust at all {", ParseOptions::default()).is_none());
    }

    #[test]
    fn empty_file_returns_none() {
        assert!(render_str("", ParseOptions::default()).is_none());
    }
}
