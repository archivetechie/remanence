//! I1: the single-writer census, a permanent merge-gate test.
//!
//! The library runtime is the only writer of SQL authority. This test fails the build when
//! production code calls (a) the legacy audit funnel (`append_operation_audit`,
//! `append_and_project_audit`, `append_and_project_audit_locked`, or a public inherent `append*`
//! method of `FileAuditLog`), or (b) a mutating method of the projector's SQL layer (every
//! `&mut self` method of an inherent impl in `projector/index.rs` or `projector/handle.rs`, plus
//! the four apply methods that `projector/thread.rs` and `projector/tape_log_projection.rs`
//! define; the set is derived from the parse on every run), from anywhere the allowlist
//! `tests/census/single-writer.tsv` does not name. A finding is (file, enclosing function,
//! callee) with a count of calls, and the allowlist must match the count exactly: the test fails
//! on an unlisted key, on a key whose count in the code differs from the row in either direction,
//! and on a row whose site no longer exists, so the allowlist only shrinks deliberately. The
//! projector's own modules are listed by function like every other site, not exempted by
//! directory.
//!
//! It replaces the Python heuristic `census.py` with a parse. Every `src/**/*.rs` file and
//! `build.rs` of every workspace member is read with `syn`; `tests/`, `benches/` and `examples/`
//! are never read. The module tree is followed from the crate roots through `mod` declarations,
//! inline modules and `#[path]`. A file reached by several declarations takes the least
//! test-like one; a file no declaration reaches counts as production and is listed.
//!
//! # What counts as production
//!
//! `cfg` is evaluated the way `census.py` does, in three values (in, out, unknown), under two
//! configurations: shipped (`test` false, no feature) and `rem-dev` (`feature = "fixtures"`).
//! An item is test-only when the shipped configuration compiles it out and `rem-dev` does not
//! bring it back: `cfg(test)`, `all(test, ..)`, `any(test, ..)` in any order, `not(..)`, and
//! `#[test]` or `#[<path>::test]` functions. An item that only `rem-dev` brings back is
//! fixtures-only; those are reported apart and never count. `cfg_attr` never gates. A predicate
//! that cannot be evaluated, an unknown atom such as `unix`, or an attribute that does not parse,
//! counts as production (fail closed).
//!
//! One departure from `census.py`: it reads every feature except `fixtures` as off, which hides
//! code behind `feature = "tui"` (a default feature of `rem`) and `field-kit` from its census.
//! Here a feature that a workspace manifest declares is unknown, so the code behind it is
//! production; a feature declared nowhere can never be on and stays off.
//!
//! # Blind spots
//!
//! This test is a bounded guard, not a proof. It matches callee names, not types. It does not
//! resolve calls through traits, closures, function pointers or macros, and it does not follow
//! dataflow. A method that shares a name with a mutator is flagged (the gRPC client methods
//! `ack_alarm` and `retire_drive` are examples), and a call made through a re-export or a renamed
//! binding (`use x::mutate as m; m(&mut idx)`) can escape it.
//!
//! - The set of writers is the `&mut self` methods of `index.rs` and `handle.rs` and the four
//!   named methods. A writer that takes `&self`, or that is defined elsewhere, is not in it:
//!   `with_core`, rusqlite's `execute`, anything in `projector/mod.rs`, or a method in a new
//!   file.
//! - Raw rusqlite against the catalog outside the SQL layer is not seen.
//! - The `&self` projector entries `project`, `project_library` and `catch_up_library`, and the
//!   free functions `rebuild_library` and `project_library_snapshot`, are not matched.
//! - Twin names: `release_tape_io_fence`, `set_tape_generation`, `retire_tape` and the
//!   `provision_*` methods exist both as runtime-routed and as SQL-first methods, and a name
//!   match cannot tell them apart. The per-key count narrows this gap and does not close it.
//! - `apply` is not matched by name: it is a method of the projector thread, but it collides
//!   with unrelated methods (the parity codec, the tape-log fold), so a call to it is not seen.
//! - The `AuditSink::append` trait method is not in the funnel set: `append` is also the name of
//!   the library, tape-log and runtime appends, so a call through the trait is not seen.
//! - A feature that exists only implicitly, because an optional dependency of that name is
//!   declared, is not in any `[features]` table and is read as off.
//! - Macro bodies are scanned lexically for `.name(` and `name(` tokens, without `cfg`
//!   evaluation inside the macro, and calls that a macro expansion generates are not seen.
//!   `include!` is not followed. Two same-named functions in one file share a key.
//!
//! Agreement with `census.py`, or equal counts, is not a proof of completeness: the allowlist
//! records the sites this parse can see.
//!
//! `CENSUS_WORKSPACE` points the test at another copy of the sources and allowlist (the
//! planted-defect runs use it; the summary line prints the root that was checked);
//! `CENSUS_EXPORT=<dir>` writes the findings and file classes there.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Component, Path, PathBuf};

use syn::parse::{ParseStream, Parser};
use syn::punctuated::Punctuated;
use syn::visit::{self, Visit};
use syn::{
    token, Arm, Attribute, Expr, ExprCall, ExprMethodCall, FieldValue, ImplItem, Item, ItemFn,
    ItemImpl, ItemMod, Local, Macro, Meta, Pat, Signature, StmtMacro, TraitItem, Type, Visibility,
};

// ---------------------------------------------------------------------------------------------
// cfg classification

/// How far an item is from the shipped binaries (`rem`, `rem-daemon`), ordered by that distance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Class {
    /// Compiled into a shipped binary, or not provably compiled out.
    Prod,
    /// Compiled out of the shipped binaries but into `rem-dev` (`feature = "fixtures"`).
    Fixtures,
    /// Provably compiled out of every shipped binary.
    Test,
}

/// Which feature names the cfg evaluator treats as unknown instead of off. `census.py` treats
/// every feature other than `fixtures` as off in both configurations; an empty set reproduces it.
#[derive(Clone, Debug, Default)]
struct CfgEnv {
    unknown_features: BTreeSet<String>,
}

fn eval_list(
    list: &syn::MetaList,
    features_on: &[&str],
    env: &CfgEnv,
) -> Option<Vec<Option<bool>>> {
    let args = list
        .parse_args_with(Punctuated::<Meta, syn::Token![,]>::parse_terminated)
        .ok()?;
    Some(args.iter().map(|m| eval_cfg(m, features_on, env)).collect())
}

/// Three-valued cfg evaluation. `Some(true)` is compiled in, `Some(false)` compiled out, `None`
/// unknown (kept in). `test` is false in both configurations.
fn eval_cfg(meta: &Meta, features_on: &[&str], env: &CfgEnv) -> Option<bool> {
    match meta {
        Meta::Path(p) if p.is_ident("test") => Some(false),
        Meta::Path(_) => None,
        Meta::NameValue(nv) if nv.path.is_ident("feature") => {
            let Expr::Lit(lit) = &nv.value else {
                return None;
            };
            let syn::Lit::Str(name) = &lit.lit else {
                return None;
            };
            let name = name.value();
            if features_on.contains(&name.as_str()) {
                Some(true)
            } else if env.unknown_features.contains(&name) {
                None
            } else {
                Some(false)
            }
        }
        Meta::NameValue(_) => None,
        Meta::List(list) if list.path.is_ident("not") => {
            let vals = eval_list(list, features_on, env)?;
            match vals.as_slice() {
                [Some(v)] => Some(!v),
                _ => None,
            }
        }
        Meta::List(list) if list.path.is_ident("all") => {
            let vals = eval_list(list, features_on, env)?;
            if vals.contains(&Some(false)) {
                Some(false)
            } else if vals.iter().any(|v| v.is_none()) {
                None
            } else {
                Some(true)
            }
        }
        Meta::List(list) if list.path.is_ident("any") => {
            let vals = eval_list(list, features_on, env)?;
            if vals.contains(&Some(true)) {
                Some(true)
            } else if vals.iter().any(|v| v.is_none()) {
                None
            } else {
                Some(false)
            }
        }
        Meta::List(_) => None,
    }
}

/// Class of one `#[cfg(...)]` predicate: compiled out of the shipped configuration is test-only,
/// unless the fixtures configuration compiles it in, which makes it fixtures-only.
fn cfg_class(pred: &Meta, env: &CfgEnv) -> Class {
    if eval_cfg(pred, &[], env) == Some(false) {
        if eval_cfg(pred, &["fixtures"], env) == Some(true) {
            Class::Fixtures
        } else {
            Class::Test
        }
    } else {
        Class::Prod
    }
}

fn attr_class(attr: &Attribute, env: &CfgEnv) -> Class {
    let path = attr.path();
    if path.is_ident("cfg") {
        if let Meta::List(list) = &attr.meta {
            return match list.parse_args::<Meta>() {
                Ok(pred) => cfg_class(&pred, env),
                Err(_) => Class::Prod,
            };
        }
        return Class::Prod;
    }
    // `#[test]`, `#[tokio::test]`, `#[tokio::test(flavor = "multi_thread")]`. A doc comment is a
    // `doc` attribute and never counts.
    if path.segments.last().is_some_and(|s| s.ident == "test") {
        return Class::Test;
    }
    Class::Prod
}

fn attrs_class(attrs: &[Attribute], env: &CfgEnv) -> Class {
    attrs
        .iter()
        .map(|a| attr_class(a, env))
        .max()
        .unwrap_or(Class::Prod)
}

// ---------------------------------------------------------------------------------------------
// attribute accessors (syn exposes none; an unknown variant must fail loudly, not fall open)

macro_rules! attrs_of {
    ($value:expr, $what:literal, $($variant:path),+ $(,)?) => {
        match $value {
            $($variant(x) => &x.attrs[..],)+
            _ => panic!(concat!("unhandled syn ", $what, " variant: extend the census")),
        }
    };
}

fn item_attrs(i: &Item) -> &[Attribute] {
    if matches!(i, Item::Verbatim(_)) {
        return &[];
    }
    attrs_of!(
        i,
        "Item",
        Item::Const,
        Item::Enum,
        Item::ExternCrate,
        Item::Fn,
        Item::ForeignMod,
        Item::Impl,
        Item::Macro,
        Item::Mod,
        Item::Static,
        Item::Struct,
        Item::Trait,
        Item::TraitAlias,
        Item::Type,
        Item::Union,
        Item::Use,
    )
}

fn impl_item_attrs(i: &ImplItem) -> &[Attribute] {
    if matches!(i, ImplItem::Verbatim(_)) {
        return &[];
    }
    attrs_of!(
        i,
        "ImplItem",
        ImplItem::Const,
        ImplItem::Fn,
        ImplItem::Type,
        ImplItem::Macro
    )
}

fn trait_item_attrs(i: &TraitItem) -> &[Attribute] {
    if matches!(i, TraitItem::Verbatim(_)) {
        return &[];
    }
    attrs_of!(
        i,
        "TraitItem",
        TraitItem::Const,
        TraitItem::Fn,
        TraitItem::Type,
        TraitItem::Macro
    )
}

fn expr_attrs(e: &Expr) -> &[Attribute] {
    if matches!(e, Expr::Verbatim(_)) {
        return &[];
    }
    attrs_of!(
        e,
        "Expr",
        Expr::Array,
        Expr::Assign,
        Expr::Async,
        Expr::Await,
        Expr::Binary,
        Expr::Block,
        Expr::Break,
        Expr::Call,
        Expr::Cast,
        Expr::Closure,
        Expr::Const,
        Expr::Continue,
        Expr::Field,
        Expr::ForLoop,
        Expr::Group,
        Expr::If,
        Expr::Index,
        Expr::Infer,
        Expr::Let,
        Expr::Lit,
        Expr::Loop,
        Expr::Macro,
        Expr::Match,
        Expr::MethodCall,
        Expr::Paren,
        Expr::Path,
        Expr::Range,
        Expr::RawAddr,
        Expr::Reference,
        Expr::Repeat,
        Expr::Return,
        Expr::Struct,
        Expr::Try,
        Expr::TryBlock,
        Expr::Tuple,
        Expr::Unary,
        Expr::Unsafe,
        Expr::While,
        Expr::Yield,
    )
}

// ---------------------------------------------------------------------------------------------
// macro token scanning (no cfg evaluation inside a macro; see the module doc)

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Form {
    /// `receiver.name(...)`.
    Method,
    /// `name(...)` (`multi` is false) or `a::b::name(...)` (`multi` is true).
    Path { multi: bool },
}

enum Tok {
    Ident(String),
    Punct(char),
    Colon2,
    Dot,
    Other,
}

fn next_token(input: ParseStream) -> syn::Result<Tok> {
    input.step(|cursor| {
        if let Some((ident, rest)) = cursor.ident() {
            return Ok((Tok::Ident(ident.to_string()), rest));
        }
        if let Some((punct, rest)) = cursor.punct() {
            let ch = punct.as_char();
            if ch == '.' {
                // `..`, `..=` and `...` are ranges, not method-call dots.
                let mut rest = rest;
                let mut range = false;
                while let Some((q, r)) = rest.punct() {
                    if q.as_char() == '.' || (range && q.as_char() == '=') {
                        range = true;
                        rest = r;
                    } else {
                        break;
                    }
                }
                return Ok((if range { Tok::Other } else { Tok::Dot }, rest));
            }
            if ch == ':' {
                if let Some((q, r)) = rest.punct() {
                    if q.as_char() == ':' {
                        return Ok((Tok::Colon2, r));
                    }
                }
            }
            return Ok((Tok::Punct(ch), rest));
        }
        match cursor.token_tree() {
            Some((_, rest)) => Ok((Tok::Other, rest)),
            None => Err(cursor.error("end of tokens")),
        }
    })
}

fn scan_stream(input: ParseStream, out: &mut Vec<(String, Form)>) -> syn::Result<()> {
    #[derive(PartialEq)]
    enum Prev {
        Dot,
        Colon2,
        Fn,
        Other,
    }
    let mut prev = Prev::Other;
    // A method name seen after `.`, waiting for its turbofish to close and a `(`.
    let mut turbofish: Option<(String, i32, char)> = None;
    while !input.is_empty() {
        if input.peek(token::Paren) {
            let inner;
            syn::parenthesized!(inner in input);
            scan_stream(&inner, out)?;
            prev = Prev::Other;
            continue;
        }
        if input.peek(token::Bracket) {
            let inner;
            syn::bracketed!(inner in input);
            scan_stream(&inner, out)?;
            prev = Prev::Other;
            continue;
        }
        if input.peek(token::Brace) {
            let inner;
            syn::braced!(inner in input);
            scan_stream(&inner, out)?;
            prev = Prev::Other;
            continue;
        }
        let tok = next_token(input)?;
        if let Some((name, depth, last)) = turbofish.as_mut() {
            match &tok {
                Tok::Punct('<') => *depth += 1,
                Tok::Punct('>') if *last != '-' => *depth -= 1,
                _ => {}
            }
            *last = match &tok {
                Tok::Punct(c) => *c,
                _ => ' ',
            };
            if *depth <= 0 && !matches!(tok, Tok::Colon2) {
                let name = std::mem::take(name);
                turbofish = None;
                if input.peek(token::Paren) {
                    out.push((name, Form::Method));
                }
            }
            continue;
        }
        match tok {
            Tok::Ident(name) => {
                if prev == Prev::Dot {
                    if input.peek(token::Paren) {
                        out.push((name, Form::Method));
                    } else if input.peek(syn::Token![::]) {
                        turbofish = Some((name, 0, ' '));
                    }
                    prev = Prev::Other;
                } else if name == "fn" {
                    prev = Prev::Fn;
                } else {
                    if prev != Prev::Fn && input.peek(token::Paren) {
                        out.push((
                            name,
                            Form::Path {
                                multi: prev == Prev::Colon2,
                            },
                        ));
                    }
                    prev = Prev::Other;
                }
            }
            Tok::Dot => prev = Prev::Dot,
            Tok::Colon2 => prev = Prev::Colon2,
            Tok::Punct(_) | Tok::Other => prev = Prev::Other,
        }
    }
    Ok(())
}

/// Scan a macro's token stream. A macro (not a function) because the token type belongs to
/// `proc-macro2`, which this test deliberately does not name as a dependency.
macro_rules! scan {
    ($tokens:expr) => {{
        let mut found: Vec<(String, Form)> = Vec::new();
        Parser::parse2(|input: ParseStream| scan_stream(input, &mut found), $tokens)
            .unwrap_or_else(|e| panic!("macro token scan failed: {e}"));
        found
    }};
}

// ---------------------------------------------------------------------------------------------
// the walk: calls, module declarations and method definitions, each with its cfg class

/// One call found in a file.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Call {
    class: Class,
    func: String,
    callee: String,
    form: Form,
}

/// A `mod name;` declaration.
#[derive(Clone, Debug)]
struct ModDecl {
    name: String,
    path: Option<String>,
    /// Class of the declaration inside its file (the file's own class is applied later).
    class: Class,
    /// Enclosing inline modules, outermost first: (name, `#[path]`).
    inline: Vec<(String, Option<String>)>,
}

/// A method defined in an `impl` block.
#[derive(Clone, Debug)]
struct Def {
    owner: String,
    trait_impl: bool,
    name: String,
    class: Class,
    public: bool,
    mut_self: bool,
}

#[derive(Default)]
struct Walk {
    env: CfgEnv,
    stack: Vec<Class>,
    funcs: Vec<String>,
    mods: Vec<(String, Option<String>)>,
    impls: Vec<(String, bool)>,
    calls: Vec<Call>,
    decls: Vec<ModDecl>,
    defs: Vec<Def>,
    /// Labelled class of every gated node, for the cfg-classifier tests.
    trace: Option<Vec<(String, Class)>>,
}

const OUTSIDE_FN: &str = "<outside-fn>";

impl Walk {
    fn new(env: &CfgEnv, base: Class) -> Walk {
        Walk {
            env: env.clone(),
            stack: vec![base],
            ..Walk::default()
        }
    }

    fn class(&self) -> Class {
        *self.stack.last().expect("class stack is never empty")
    }

    fn func(&self) -> String {
        self.funcs
            .last()
            .cloned()
            .unwrap_or_else(|| OUTSIDE_FN.to_string())
    }

    /// Run `f` with the class raised by `attrs`. An item is as far from production as its
    /// enclosing scope or its own attributes make it.
    fn gated(
        &mut self,
        attrs: &[Attribute],
        label: impl FnOnce() -> String,
        f: impl FnOnce(&mut Self),
    ) {
        let class = self.class().max(attrs_class(attrs, &self.env));
        self.stack.push(class);
        if let Some(trace) = self.trace.as_mut() {
            trace.push((label(), class));
        }
        f(self);
        self.stack.pop();
    }

    fn record(&mut self, callee: String, form: Form) {
        let call = Call {
            class: self.class(),
            func: self.func(),
            callee,
            form,
        };
        self.calls.push(call);
    }

    fn record_scan(&mut self, found: Vec<(String, Form)>) {
        for (callee, form) in found {
            self.record(callee, form);
        }
    }
}

fn path_attr(attrs: &[Attribute]) -> Option<String> {
    attrs.iter().find_map(|a| match &a.meta {
        Meta::NameValue(nv) if nv.path.is_ident("path") => match &nv.value {
            Expr::Lit(l) => match &l.lit {
                syn::Lit::Str(s) => Some(s.value()),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    })
}

fn type_name(t: &Type) -> String {
    match t {
        Type::Path(p) => p
            .path
            .segments
            .last()
            .map_or_else(|| "?".to_string(), |s| s.ident.to_string()),
        Type::Reference(r) => type_name(&r.elem),
        _ => "?".to_string(),
    }
}

fn is_mut_self(sig: &Signature) -> bool {
    match sig.receiver() {
        Some(r) => {
            (r.reference.is_some() && r.mutability.is_some())
                || (r.colon_token.is_some()
                    && matches!(&*r.ty, Type::Reference(t) if t.mutability.is_some()))
        }
        None => false,
    }
}

fn use_head(tree: &syn::UseTree) -> String {
    match tree {
        syn::UseTree::Path(p) => p.ident.to_string(),
        syn::UseTree::Name(n) => n.ident.to_string(),
        syn::UseTree::Rename(r) => r.ident.to_string(),
        syn::UseTree::Glob(_) => "*".to_string(),
        syn::UseTree::Group(_) => "{..}".to_string(),
    }
}

fn item_label(i: &Item) -> String {
    match i {
        Item::Fn(f) => format!("fn {}", f.sig.ident),
        Item::Mod(m) => format!("mod {}", m.ident),
        Item::Use(u) => format!("use {}", use_head(&u.tree)),
        Item::Struct(s) => format!("struct {}", s.ident),
        Item::Enum(e) => format!("enum {}", e.ident),
        Item::Impl(_) => "impl".to_string(),
        Item::Const(c) => format!("const {}", c.ident),
        Item::Static(s) => format!("static {}", s.ident),
        Item::Trait(t) => format!("trait {}", t.ident),
        Item::Type(t) => format!("type {}", t.ident),
        _ => "item".to_string(),
    }
}

impl<'ast> Visit<'ast> for Walk {
    fn visit_item(&mut self, i: &'ast Item) {
        let attrs = item_attrs(i);
        self.gated(
            attrs,
            || item_label(i),
            |s| match i {
                Item::Verbatim(tokens) => {
                    let found = scan!(tokens.clone());
                    s.record_scan(found);
                }
                _ => visit::visit_item(s, i),
            },
        );
    }

    fn visit_item_fn(&mut self, f: &'ast ItemFn) {
        self.funcs.push(f.sig.ident.to_string());
        visit::visit_item_fn(self, f);
        self.funcs.pop();
    }

    fn visit_item_mod(&mut self, m: &'ast ItemMod) {
        let path = path_attr(&m.attrs);
        if m.content.is_some() {
            self.mods.push((m.ident.to_string(), path));
            visit::visit_item_mod(self, m);
            self.mods.pop();
        } else {
            let decl = ModDecl {
                name: m.ident.to_string(),
                path,
                class: self.class(),
                inline: self.mods.clone(),
            };
            self.decls.push(decl);
        }
    }

    fn visit_item_impl(&mut self, i: &'ast ItemImpl) {
        self.impls.push((type_name(&i.self_ty), i.trait_.is_some()));
        visit::visit_item_impl(self, i);
        self.impls.pop();
    }

    fn visit_impl_item(&mut self, i: &'ast ImplItem) {
        let attrs = impl_item_attrs(i);
        self.gated(
            attrs,
            || match i {
                ImplItem::Fn(f) => format!("fn {}", f.sig.ident),
                _ => "impl-item".to_string(),
            },
            |s| match i {
                ImplItem::Verbatim(tokens) => {
                    let found = scan!(tokens.clone());
                    s.record_scan(found);
                }
                ImplItem::Fn(f) => {
                    let (owner, trait_impl) =
                        s.impls.last().cloned().unwrap_or(("?".to_string(), false));
                    s.defs.push(Def {
                        owner,
                        trait_impl,
                        name: f.sig.ident.to_string(),
                        class: s.class(),
                        public: !matches!(f.vis, Visibility::Inherited),
                        mut_self: is_mut_self(&f.sig),
                    });
                    s.funcs.push(f.sig.ident.to_string());
                    visit::visit_impl_item(s, i);
                    s.funcs.pop();
                }
                _ => visit::visit_impl_item(s, i),
            },
        );
    }

    fn visit_trait_item(&mut self, i: &'ast TraitItem) {
        let attrs = trait_item_attrs(i);
        self.gated(
            attrs,
            || match i {
                TraitItem::Fn(f) => format!("fn {}", f.sig.ident),
                _ => "trait-item".to_string(),
            },
            |s| match i {
                TraitItem::Verbatim(tokens) => {
                    let found = scan!(tokens.clone());
                    s.record_scan(found);
                }
                TraitItem::Fn(f) => {
                    s.funcs.push(f.sig.ident.to_string());
                    visit::visit_trait_item(s, i);
                    s.funcs.pop();
                }
                _ => visit::visit_trait_item(s, i),
            },
        );
    }

    // Attributes on items and on expression statements sit on the inner node, which `visit_item`
    // and `visit_expr` gate. `let` and statement macros carry their own, gated here.
    fn visit_local(&mut self, l: &'ast Local) {
        self.gated(
            &l.attrs,
            || match &l.pat {
                Pat::Ident(p) => format!("let {}", p.ident),
                _ => "let".to_string(),
            },
            |s| visit::visit_local(s, l),
        );
    }

    fn visit_stmt_macro(&mut self, m: &'ast StmtMacro) {
        self.gated(
            &m.attrs,
            || "stmt-macro".to_string(),
            |s| visit::visit_stmt_macro(s, m),
        );
    }

    fn visit_arm(&mut self, a: &'ast Arm) {
        self.gated(&a.attrs, || "arm".to_string(), |s| visit::visit_arm(s, a));
    }

    fn visit_field_value(&mut self, f: &'ast FieldValue) {
        self.gated(
            &f.attrs,
            || "field".to_string(),
            |s| visit::visit_field_value(s, f),
        );
    }

    fn visit_expr(&mut self, e: &'ast Expr) {
        let attrs = expr_attrs(e);
        self.gated(
            attrs,
            || "expr".to_string(),
            |s| match e {
                Expr::Verbatim(tokens) => {
                    let found = scan!(tokens.clone());
                    s.record_scan(found);
                }
                _ => visit::visit_expr(s, e),
            },
        );
    }

    fn visit_expr_method_call(&mut self, m: &'ast ExprMethodCall) {
        self.record(m.method.to_string(), Form::Method);
        visit::visit_expr_method_call(self, m);
    }

    fn visit_expr_call(&mut self, c: &'ast ExprCall) {
        if let Expr::Path(p) = &*c.func {
            if let Some(last) = p.path.segments.last() {
                let multi = p.path.segments.len() > 1 || p.qself.is_some();
                self.record(last.ident.to_string(), Form::Path { multi });
            }
        }
        visit::visit_expr_call(self, c);
    }

    fn visit_macro(&mut self, m: &'ast Macro) {
        let found = scan!(m.tokens.clone());
        self.record_scan(found);
    }
}

/// Everything one file contributes.
struct Walked {
    /// Class from the file's own inner attributes (`#![cfg(test)]`).
    inner_class: Class,
    /// Every call, repeats included: the allowlist counts calls per key.
    calls: Vec<Call>,
    decls: Vec<ModDecl>,
    defs: Vec<Def>,
    trace: Vec<(String, Class)>,
}

fn walk_file(file: &syn::File, env: &CfgEnv, base: Class, trace: bool) -> Walked {
    let inner_class = attrs_class(&file.attrs, env);
    let mut walk = Walk::new(env, base.max(inner_class));
    if trace {
        walk.trace = Some(Vec::new());
    }
    for item in &file.items {
        walk.visit_item(item);
    }
    Walked {
        inner_class,
        calls: walk.calls,
        decls: walk.decls,
        defs: walk.defs,
        trace: walk.trace.unwrap_or_default(),
    }
}

// ---------------------------------------------------------------------------------------------
// the workspace: files, the module tree, and each file's class

const ALLOWLIST: &str = "crates/remanence-state/tests/census/single-writer.tsv";
const INDEX_RS: &str = "crates/remanence-state/src/projector/index.rs";
const HANDLE_RS: &str = "crates/remanence-state/src/projector/handle.rs";
const AUDIT_RS: &str = "crates/remanence-state/src/audit.rs";
const THREAD_RS: &str = "crates/remanence-state/src/projector/thread.rs";
const TAPE_LOG_PROJECTION_RS: &str = "crates/remanence-state/src/projector/tape_log_projection.rs";
/// The `&mut self` methods that apply authority from `thread.rs` and `tape_log_projection.rs`,
/// named one by one (D-5-1). Each is looked up in the parse and must still exist. `apply` is
/// left out on purpose: it collides with unrelated methods by name. `finish_observed_fold` is
/// defined under `cfg(test)`; it is listed all the same.
const PROJECTOR_APPLY_METHODS: [&str; 4] = [
    "apply_frozen",
    "project_tape_log_bounded",
    "clear_typed_projection",
    "finish_observed_fold",
];
/// The legacy audit funnel's free functions, `append_and_project_audit_locked` being its
/// production entry. The `FileAuditLog` append methods are derived.
const FUNNEL_FUNCTIONS: [&str; 3] = [
    "append_operation_audit",
    "append_and_project_audit",
    "append_and_project_audit_locked",
];

fn workspace_root() -> PathBuf {
    std::env::var_os("CENSUS_WORKSPACE")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .canonicalize()
        .expect("workspace root")
}

fn normalized(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir if out.file_name().is_some_and(|n| n != "..") => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

fn rel_string(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Whether a file's children live next to it (`lib.rs`, `main.rs`, `mod.rs`, a crate root, a
/// `#[path]` file) or in a directory named after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Owns {
    Dir,
    StemDir,
}

fn owns_by_name(path: &Path) -> Owns {
    match path.file_name().and_then(|n| n.to_str()) {
        Some("lib.rs" | "main.rs" | "mod.rs" | "build.rs") => Owns::Dir,
        _ => Owns::StemDir,
    }
}

struct Parsed {
    walked: Walked,
}

struct Crate {
    /// Source files (`src/**/*.rs`, `build.rs`) and their parse (or its error).
    files: BTreeMap<PathBuf, Result<Parsed, String>>,
    /// Crate roots and the class each is built in.
    roots: Vec<(PathBuf, Class)>,
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|e| e.expect("dir entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().is_some_and(|x| x == "rs") {
            out.push(path);
        }
    }
}

fn workspace_members(root: &Path) -> Vec<PathBuf> {
    let text = fs::read_to_string(root.join("Cargo.toml")).expect("workspace Cargo.toml");
    let table: toml::Table = text.parse().expect("workspace Cargo.toml parses");
    let members: Vec<PathBuf> = table["workspace"]["members"]
        .as_array()
        .expect("workspace.members")
        .iter()
        .map(|m| PathBuf::from(m.as_str().expect("member path")))
        .collect();
    // Fail closed: a crate directory that is not a listed member would be skipped silently.
    let listed: BTreeSet<_> = members.iter().map(|m| normalized(m)).collect();
    if let Ok(dir) = fs::read_dir(root.join("crates")) {
        for entry in dir.map(|e| e.expect("dir entry").path()) {
            if entry.join("Cargo.toml").is_file() {
                let rel = entry.strip_prefix(root).expect("under root").to_path_buf();
                assert!(
                    listed.contains(&rel),
                    "{} has a Cargo.toml but is not a workspace member: add it to the \
                     workspace or to this census",
                    rel.display()
                );
            }
        }
    }
    members
}

/// Crate roots from the manifest: the default targets plus any explicit `path`. A target that
/// requires only `fixtures` is built in the fixtures configuration.
fn crate_roots(root: &Path, member: &Path) -> Vec<(PathBuf, Class)> {
    let manifest = fs::read_to_string(root.join(member).join("Cargo.toml"))
        .unwrap_or_else(|e| panic!("{} manifest: {e}", member.display()));
    let table: toml::Table = manifest.parse().expect("member Cargo.toml parses");
    let mut roots: BTreeMap<PathBuf, Class> = BTreeMap::new();
    let mut add = |path: PathBuf, class: Class| {
        roots
            .entry(normalized(&member.join(path)))
            .and_modify(|c| *c = (*c).min(class))
            .or_insert(class);
    };
    let src = root.join(member).join("src");
    for default in ["src/lib.rs", "src/main.rs", "build.rs"] {
        if root.join(member).join(default).is_file() {
            add(PathBuf::from(default), Class::Prod);
        }
    }
    if let Ok(dir) = fs::read_dir(src.join("bin")) {
        for entry in dir.map(|e| e.expect("dir entry").path()) {
            if entry.extension().is_some_and(|x| x == "rs") {
                add(
                    PathBuf::from("src/bin").join(entry.file_name().unwrap()),
                    Class::Prod,
                );
            } else if entry.join("main.rs").is_file() {
                add(
                    PathBuf::from("src/bin")
                        .join(entry.file_name().unwrap())
                        .join("main.rs"),
                    Class::Prod,
                );
            }
        }
    }
    if let Some(lib) = table
        .get("lib")
        .and_then(|l| l.get("path"))
        .and_then(|p| p.as_str())
    {
        add(PathBuf::from(lib), Class::Prod);
    }
    for bin in table
        .get("bin")
        .and_then(|b| b.as_array())
        .into_iter()
        .flatten()
    {
        if let Some(path) = bin.get("path").and_then(|p| p.as_str()) {
            let required: Vec<&str> = bin
                .get("required-features")
                .and_then(|r| r.as_array())
                .map(|a| a.iter().filter_map(|f| f.as_str()).collect())
                .unwrap_or_default();
            let class = if !required.is_empty() && required.iter().all(|f| *f == "fixtures") {
                Class::Fixtures
            } else {
                Class::Prod
            };
            add(PathBuf::from(path), class);
        }
    }
    roots.into_iter().collect()
}

fn load_crate(root: &Path, member: &Path, env: &CfgEnv) -> Crate {
    let mut paths = Vec::new();
    collect_rs(&root.join(member).join("src"), &mut paths);
    if root.join(member).join("build.rs").is_file() {
        paths.push(root.join(member).join("build.rs"));
    }
    let mut files = BTreeMap::new();
    for abs in paths {
        let rel = abs.strip_prefix(root).expect("under root").to_path_buf();
        let parsed = fs::read_to_string(&abs)
            .map_err(|e| format!("read: {e}"))
            .and_then(|text| syn::parse_file(&text).map_err(|e| format!("parse: {e}")))
            .map(|ast| Parsed {
                walked: walk_file(&ast, env, Class::Prod, false),
            });
        files.insert(rel, parsed);
    }
    Crate {
        files,
        roots: crate_roots(root, member),
    }
}

/// Candidate files for a `mod name;` declaration, per the Rust reference's module-path rules.
fn mod_candidates(file: &Path, owns: Owns, decl: &ModDecl) -> Vec<PathBuf> {
    let dir = file.parent().unwrap_or(Path::new(""));
    let mut base = match owns {
        Owns::Dir => dir.to_path_buf(),
        Owns::StemDir => dir.join(file.file_stem().expect("file stem")),
    };
    if let (Some(p), true) = (&decl.path, decl.inline.is_empty()) {
        return vec![normalized(&dir.join(p))];
    }
    for (name, path) in &decl.inline {
        base = base.join(path.as_deref().unwrap_or(name));
    }
    match &decl.path {
        Some(p) => vec![normalized(&base.join(p))],
        None => vec![
            normalized(&base.join(format!("{}.rs", decl.name))),
            normalized(&base.join(&decl.name).join("mod.rs")),
        ],
    }
}

/// Class of every source file, found by following `mod` declarations from the crate roots. A
/// file declared more than once takes its least test-like declaration. A file no declaration
/// reaches is treated as production (fail closed) and listed in `orphans`.
struct Classes {
    class: BTreeMap<PathBuf, Class>,
    orphans: Vec<PathBuf>,
    unresolved: Vec<String>,
}

fn classify_files(krate: &Crate) -> Classes {
    let mut state: BTreeMap<PathBuf, (Class, Owns)> = BTreeMap::new();
    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    let mut unresolved = Vec::new();
    let relax = |state: &mut BTreeMap<PathBuf, (Class, Owns)>,
                 queue: &mut VecDeque<PathBuf>,
                 file: PathBuf,
                 class: Class,
                 owns: Owns| {
        match state.get(&file) {
            Some((old, _)) if *old <= class => {}
            _ => {
                state.insert(file.clone(), (class, owns));
                queue.push_back(file);
            }
        }
    };
    for (root, class) in &krate.roots {
        if krate.files.contains_key(root) {
            relax(&mut state, &mut queue, root.clone(), *class, Owns::Dir);
        }
    }
    let mut orphans = Vec::new();
    loop {
        while let Some(file) = queue.pop_front() {
            let (class, owns) = state[&file];
            let Some(Ok(parsed)) = krate.files.get(&file) else {
                continue;
            };
            for decl in &parsed.walked.decls {
                let effective = class.max(parsed.walked.inner_class).max(decl.class);
                let found = mod_candidates(&file, owns, decl)
                    .into_iter()
                    .find(|c| krate.files.contains_key(c));
                match found {
                    Some(child) => {
                        let child_owns = if decl.path.is_some() {
                            Owns::Dir
                        } else {
                            owns_by_name(&child)
                        };
                        relax(&mut state, &mut queue, child, effective, child_owns);
                    }
                    None if effective == Class::Test => {}
                    None => unresolved.push(format!(
                        "{}: `mod {};` resolves to no source file",
                        rel_string(&file),
                        decl.name
                    )),
                }
            }
        }
        let next = krate
            .files
            .keys()
            .find(|f| !state.contains_key(*f))
            .cloned();
        match next {
            Some(file) => {
                orphans.push(file.clone());
                let owns = owns_by_name(&file);
                relax(&mut state, &mut queue, file, Class::Prod, owns);
            }
            None => break,
        }
    }
    Classes {
        class: state.into_iter().map(|(f, (c, _))| (f, c)).collect(),
        orphans,
        unresolved,
    }
}

// ---------------------------------------------------------------------------------------------
// findings

/// What a finding is a call to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Funnel,
    Mutator,
    Both,
}

impl Kind {
    fn describe(self) -> &'static str {
        match self {
            Kind::Funnel => "legacy audit funnel",
            Kind::Mutator => "SQL-layer mutator",
            Kind::Both => "legacy audit funnel and SQL-layer mutator",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    file: String,
    func: String,
    callee: String,
}

/// What the census found under one key: what is called, and how many calls there are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Found {
    kind: Kind,
    count: usize,
}

#[derive(Default)]
struct Report {
    /// Production findings, which the allowlist must name, with their exact counts.
    prod: BTreeMap<Key, Found>,
    /// Fixtures-only findings, reported and never counted.
    fixtures: BTreeMap<Key, Found>,
    funnel_names: BTreeSet<String>,
    mutator_names: BTreeSet<String>,
    file_classes: BTreeMap<String, Class>,
    orphans: Vec<String>,
    unresolved: Vec<String>,
    parse_errors: Vec<String>,
}

/// The callee names the census looks for.
struct Names {
    funnel: BTreeSet<String>,
    mutator: BTreeSet<String>,
}

/// Sort one file's calls into production and fixtures-only findings. `file_class` is the class
/// the module tree gives the whole file; a test-only file contributes nothing.
fn findings(
    rel: &str,
    file_class: Class,
    calls: &[Call],
    names: &Names,
    prod: &mut BTreeMap<Key, Found>,
    fixtures: &mut BTreeMap<Key, Found>,
) {
    for call in calls {
        let funnel = names.funnel.contains(&call.callee);
        // A mutator is a method; `Type::method(&mut x)` is the same call spelled out.
        let mutator = names.mutator.contains(&call.callee)
            && match call.form {
                Form::Method => true,
                Form::Path { multi } => multi,
            };
        let kind = match (funnel, mutator) {
            (true, true) => Kind::Both,
            (true, false) => Kind::Funnel,
            (false, true) => Kind::Mutator,
            (false, false) => continue,
        };
        let key = Key {
            file: rel.to_string(),
            func: call.func.clone(),
            callee: call.callee.clone(),
        };
        let into = match call.class.max(file_class) {
            Class::Prod => &mut *prod,
            Class::Fixtures => &mut *fixtures,
            Class::Test => continue,
        };
        let found = into.entry(key).or_insert(Found { kind, count: 0 });
        found.kind = found.kind.max(kind);
        found.count += 1;
    }
}

struct Workspace {
    krates: Vec<Crate>,
}

impl Workspace {
    fn load(root: &Path, env: &CfgEnv) -> Workspace {
        Workspace {
            krates: workspace_members(root)
                .iter()
                .map(|m| load_crate(root, m, env))
                .collect(),
        }
    }

    fn defs(&self, file: &str) -> &[Def] {
        let found = self
            .krates
            .iter()
            .find_map(|k| k.files.get(Path::new(file)))
            .unwrap_or_else(|| panic!("{file} is not a workspace source file"));
        match found {
            Ok(parsed) => &parsed.walked.defs,
            Err(e) => panic!("{file} does not parse: {e}"),
        }
    }

    /// The callee names, derived from the parse each time.
    ///
    /// Mutators: every `&mut self` method of an inherent impl in `index.rs` and `handle.rs`, any
    /// visibility, plus the four named apply methods of `thread.rs` and
    /// `tape_log_projection.rs`. Funnel: the two legacy free functions, plus the public inherent `append*`
    /// methods of `FileAuditLog`. The `AuditSink::append` trait method is left out: `append` is
    /// the name of the library, tape-log and runtime appends and of `OpenOptions::append`, so a
    /// name match flags dozens of calls that have nothing to do with the audit log. A call
    /// through the trait is one of the blind spots in the module doc.
    fn names(&self) -> Names {
        let mut mutator = BTreeSet::new();
        for file in [INDEX_RS, HANDLE_RS] {
            for d in self.defs(file) {
                if d.class != Class::Test && !d.trait_impl && d.mut_self {
                    mutator.insert(d.name.clone());
                }
            }
        }
        for file in [THREAD_RS, TAPE_LOG_PROJECTION_RS] {
            for d in self.defs(file) {
                // Named one by one, so any class: `finish_observed_fold` is defined under
                // `cfg(test)`, and a call to it from production code is still a finding.
                if !d.trait_impl && d.mut_self && PROJECTOR_APPLY_METHODS.contains(&d.name.as_str())
                {
                    mutator.insert(d.name.clone());
                }
            }
        }
        for name in PROJECTOR_APPLY_METHODS {
            assert!(
                mutator.contains(name),
                "`{name}` is no longer a `&mut self` method in thread.rs or \
                 tape_log_projection.rs: update PROJECTOR_APPLY_METHODS"
            );
        }
        let mut funnel: BTreeSet<String> = FUNNEL_FUNCTIONS.iter().map(|s| s.to_string()).collect();
        for d in self.defs(AUDIT_RS) {
            if d.class != Class::Test
                && d.owner == "FileAuditLog"
                && !d.trait_impl
                && d.public
                && d.name.starts_with("append")
            {
                funnel.insert(d.name.clone());
            }
        }
        assert!(
            !mutator.is_empty() && funnel.len() > FUNNEL_FUNCTIONS.len(),
            "the method sets derived from index.rs, handle.rs and audit.rs are empty: \
             the parse found no `&mut self` methods or no FileAuditLog append methods"
        );
        Names { funnel, mutator }
    }

    fn analyse(&self) -> Report {
        let mut report = Report::default();
        let classes: Vec<Classes> = self.krates.iter().map(classify_files).collect();
        for c in &classes {
            report
                .orphans
                .extend(c.orphans.iter().map(|p| rel_string(p)));
            report.unresolved.extend(c.unresolved.iter().cloned());
        }
        let names = self.names();
        for (krate, classes) in self.krates.iter().zip(&classes) {
            for (file, parsed) in &krate.files {
                let rel = rel_string(file);
                let file_class = classes.class.get(file).copied().unwrap_or(Class::Prod);
                report.file_classes.insert(rel.clone(), file_class);
                match parsed {
                    Ok(p) if file_class != Class::Test => findings(
                        &rel,
                        file_class,
                        &p.walked.calls,
                        &names,
                        &mut report.prod,
                        &mut report.fixtures,
                    ),
                    Ok(_) => {}
                    Err(e) if file_class != Class::Test => {
                        report.parse_errors.push(format!("{rel}: {e}"));
                    }
                    Err(_) => {}
                }
            }
        }
        report.funnel_names = names.funnel;
        report.mutator_names = names.mutator;
        report
    }
}

// ---------------------------------------------------------------------------------------------
// the allowlist

const REASONS: [&str; 6] = [
    "applies-authority",
    "unreachable-at-runtime",
    "cache",
    "pending-order",
    "routes-to-runtime",
    "name-collision",
];

#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    count: usize,
    reason: String,
    removed_by: String,
}

/// Rows are `file<TAB>enclosing_fn<TAB>callee<TAB>count<TAB>reason<TAB>removed_by`; `#` starts a
/// comment. `count` is the exact number of calls to `callee` in that function.
fn parse_allowlist(text: &str) -> Result<BTreeMap<Key, Row>, String> {
    let mut rows = BTreeMap::new();
    for (n, line) in text.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        let at = n + 1;
        let [file, func, callee, count, reason, removed_by] = fields[..] else {
            return Err(format!(
                "allowlist line {at}: expected 6 tab-separated fields \
                 (file, enclosing_fn, callee, count, reason, removed_by), found {}",
                fields.len()
            ));
        };
        let count: usize = match count.parse() {
            Ok(n) if n >= 1 => n,
            _ => {
                return Err(format!(
                    "allowlist line {at}: count `{count}` is not a whole number of at least 1"
                ))
            }
        };
        if !REASONS.contains(&reason) {
            return Err(format!(
                "allowlist line {at}: reason `{reason}` is not one of {REASONS:?}"
            ));
        }
        if removed_by.trim().is_empty() {
            return Err(format!(
                "allowlist line {at}: removed_by is empty; name the order that deletes the site, \
                 or write `permanent`"
            ));
        }
        let key = Key {
            file: file.to_string(),
            func: func.to_string(),
            callee: callee.to_string(),
        };
        let row = Row {
            count,
            reason: reason.to_string(),
            removed_by: removed_by.to_string(),
        };
        if rows.insert(key, row).is_some() {
            return Err(format!(
                "allowlist line {at}: duplicate row for {file} :: {func} :: {callee}"
            ));
        }
    }
    Ok(rows)
}

/// Every way the production findings and the allowlist disagree, as readable lines. A listed
/// count must match the code exactly, in either direction.
fn compare(found: &BTreeMap<Key, Found>, allow: &BTreeMap<Key, Row>) -> Vec<String> {
    let mut problems = Vec::new();
    for (key, f) in found {
        match allow.get(key) {
            None => problems.push(format!(
                "UNLISTED production call to the {}: `{}` called {} time(s) in fn `{}` ({}). \
                 Remove the call, or add a row to {ALLOWLIST} with its count, a reason and the \
                 order that removes it.",
                f.kind.describe(),
                key.callee,
                f.count,
                key.func,
                key.file
            )),
            Some(row) if row.count != f.count => problems.push(format!(
                "COUNT MISMATCH: `{}` is called {} time(s) in fn `{}` ({}) but the allowlist \
                 lists {}. {}",
                key.callee,
                f.count,
                key.func,
                key.file,
                row.count,
                if f.count > row.count {
                    "A call was added: remove it, or raise the count with a reason."
                } else {
                    "A call was removed: lower the count; the allowlist only shrinks."
                }
            )),
            Some(_) => {}
        }
    }
    for key in allow.keys() {
        if !found.contains_key(key) {
            problems.push(format!(
                "STALE allowlist row: `{}` is no longer called in fn `{}` ({}). Delete the row; \
                 the allowlist only shrinks.",
                key.callee, key.func, key.file
            ));
        }
    }
    problems
}

// ---------------------------------------------------------------------------------------------
// the census

fn export(report: &Report) {
    let Some(dir) = std::env::var_os("CENSUS_EXPORT") else {
        return;
    };
    let dir = PathBuf::from(dir);
    let rows = |m: &BTreeMap<Key, Found>| -> String {
        m.iter()
            .map(|(k, f)| {
                format!(
                    "{}\t{}\t{}\t{:?}\t{}\n",
                    k.file, k.func, k.callee, f.kind, f.count
                )
            })
            .collect()
    };
    fs::write(dir.join("single-writer-prod.tsv"), rows(&report.prod)).expect("export prod");
    fs::write(
        dir.join("single-writer-fixtures.tsv"),
        rows(&report.fixtures),
    )
    .expect("export fixtures");
    let files: String = report
        .file_classes
        .iter()
        .map(|(f, c)| format!("{f}\t{c:?}\n"))
        .collect();
    fs::write(dir.join("single-writer-files.tsv"), files).expect("export files");
    let names = |label: &str, set: &BTreeSet<String>| -> String {
        set.iter().map(|n| format!("{label}\t{n}\n")).collect()
    };
    fs::write(
        dir.join("single-writer-names.tsv"),
        names("mutator", &report.mutator_names) + &names("funnel", &report.funnel_names),
    )
    .expect("export names");
}

fn declared_features(root: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for member in workspace_members(root) {
        let text = fs::read_to_string(root.join(&member).join("Cargo.toml")).expect("manifest");
        let table: toml::Table = text.parse().expect("manifest parses");
        if let Some(features) = table.get("features").and_then(|f| f.as_table()) {
            out.extend(features.keys().cloned());
        }
    }
    out.remove("fixtures");
    out
}

fn run_census(root: &Path, env: &CfgEnv) -> (Report, Vec<String>) {
    let report = Workspace::load(root, env).analyse();
    let mut problems = Vec::new();
    problems.extend(
        report
            .parse_errors
            .iter()
            .map(|e| format!("UNPARSEABLE production file {e}")),
    );
    problems.extend(
        report
            .unresolved
            .iter()
            .map(|e| format!("UNRESOLVED module {e}")),
    );
    let text = fs::read_to_string(root.join(ALLOWLIST))
        .unwrap_or_else(|e| panic!("read {ALLOWLIST}: {e}"));
    match parse_allowlist(&text) {
        Ok(allow) => problems.extend(compare(&report.prod, &allow)),
        Err(e) => problems.push(e),
    }
    (report, problems)
}

#[test]
fn single_writer_census() {
    // syn's recursive descent and the AST walk both recurse deeply on the largest files.
    let outcome = std::thread::Builder::new()
        .stack_size(512 << 20)
        .spawn(|| {
            let root = workspace_root();
            let env = CfgEnv {
                unknown_features: declared_features(&root),
            };
            let outcome = run_census(&root, &env);
            (root, outcome)
        })
        .expect("spawn")
        .join()
        .expect("census thread");
    let (root, (report, problems)) = outcome;
    // The root is printed so that a CENSUS_WORKSPACE override cannot pass for the real tree.
    println!(
        "single-writer: checked {}: {} production findings ({} calls), {} fixtures-only \
         findings, {} mutator names, {} funnel names, {} orphan files",
        root.display(),
        report.prod.len(),
        report.prod.values().map(|f| f.count).sum::<usize>(),
        report.fixtures.len(),
        report.mutator_names.len(),
        report.funnel_names.len(),
        report.orphans.len()
    );
    for o in &report.orphans {
        println!("single-writer: orphan source file (scanned as production): {o}");
    }
    export(&report);
    assert!(
        problems.is_empty(),
        "single-writer census: {} problem(s)\n{}",
        problems.len(),
        problems.join("\n")
    );
}

// ---------------------------------------------------------------------------------------------
// tests of the cfg classifier: the 36 forms of `census.py selftest`
//
// Each test is one annotated line of the Python self-test (`selftest_lNN` is that line). The
// Python forms are lines of a pseudo-file; here each is a snippet of valid Rust, with the
// statement-level forms (`if`, `let`, a match arm) placed inside a function body.

fn parse(src: &str) -> syn::File {
    syn::parse_file(src).unwrap_or_else(|e| panic!("snippet does not parse: {e}\n{src}"))
}

fn walked(src: &str) -> Walked {
    walk_file(&parse(src), &CfgEnv::default(), Class::Prod, true)
}

/// The class of the node the walk labelled `label` (`fn name`, `mod name`, `let name`, ...).
fn class_of(src: &str, label: &str) -> Class {
    let trace = walked(src).trace;
    let hits: Vec<Class> = trace
        .iter()
        .filter(|(l, _)| l == label)
        .map(|(_, c)| *c)
        .collect();
    assert!(!hits.is_empty(), "no node labelled `{label}` in {trace:?}");
    assert!(
        hits.iter().all(|c| *c == hits[0]),
        "`{label}` is labelled with different classes: {hits:?}"
    );
    hits[0]
}

/// The class of the one call to `callee` in the snippet.
fn call_class(src: &str, callee: &str) -> Class {
    let calls = walked(src).calls;
    let hits: Vec<Class> = calls
        .iter()
        .filter(|c| c.callee == callee)
        .map(|c| c.class)
        .collect();
    assert!(!hits.is_empty(), "no call to `{callee}` in {calls:?}");
    assert!(
        hits.iter().all(|c| *c == hits[0]),
        "`{callee}` has different classes: {hits:?}"
    );
    hits[0]
}

#[test]
fn selftest_l03_one_line_cfg_test_use_is_test() {
    assert_eq!(
        class_of("#[cfg(test)] use a::{b, c};", "use a"),
        Class::Test
    );
}

#[test]
fn selftest_l04_fn_after_a_one_line_cfg_test_use_stays_prod() {
    let src = "#[cfg(test)] use a::{b, c};\nfn prod_after_one_liner() { let x = 1; }";
    assert_eq!(class_of(src, "fn prod_after_one_liner"), Class::Prod);
}

#[test]
fn selftest_l05_statement_inside_that_fn_stays_prod() {
    let src = "#[cfg(test)] use a::{b, c};\nfn prod_after_one_liner() { let x = 1; }";
    assert_eq!(class_of(src, "let x"), Class::Prod);
}

#[test]
fn selftest_l08_doc_comment_that_says_test_does_not_gate() {
    let src = "/// regression test for R473, keeps the test in the doc comment\n\
               pub fn prod_doc_mentions_test() {}";
    assert_eq!(class_of(src, "fn prod_doc_mentions_test"), Class::Prod);
}

#[test]
fn selftest_l10_cfg_not_test_is_prod() {
    assert_eq!(
        class_of(
            "#[cfg(not(test))] fn prod_not_test() {}",
            "fn prod_not_test"
        ),
        Class::Prod
    );
}

#[test]
fn selftest_l12_cfg_all_test_and_a_feature_is_test() {
    let src = "#[cfg(all(test, feature = \"x\"))] fn test_all_form() {}";
    assert_eq!(class_of(src, "fn test_all_form"), Class::Test);
}

#[test]
fn selftest_l14_cfg_any_fixtures_or_test_is_fixtures() {
    let src = "#[cfg(any(feature = \"fixtures\", test))] fn fixtures_reachable_in_rem_dev() {}";
    assert_eq!(
        class_of(src, "fn fixtures_reachable_in_rem_dev"),
        Class::Fixtures
    );
}

#[test]
fn selftest_l16_cfg_any_other_feature_or_test_in_either_order_is_test() {
    let src = "#[cfg(any(feature = \"other\", test))] fn test_any_reordered() {}";
    assert_eq!(class_of(src, "fn test_any_reordered"), Class::Test);
    let src = "#[cfg(any(test, feature = \"other\"))] fn test_any_reordered() {}";
    assert_eq!(class_of(src, "fn test_any_reordered"), Class::Test);
}

#[test]
fn selftest_l18_cfg_test_fn_with_a_raw_string_holding_a_brace_is_test() {
    let src = "#[cfg(test)]\nfn test_raw_string() { let s = r#\"{ not a brace \"#; }";
    assert_eq!(class_of(src, "fn test_raw_string"), Class::Test);
}

#[test]
fn selftest_l19_fn_after_a_raw_string_stays_prod() {
    let src = "#[cfg(test)]\nfn test_raw_string() { let s = r#\"{ not a brace \"#; }\n\
               fn prod_after_raw() {}";
    assert_eq!(class_of(src, "fn prod_after_raw"), Class::Prod);
}

#[test]
fn selftest_l21_cfg_test_fn_with_brace_char_literals_is_test() {
    let src = "#[cfg(test)]\nfn test_char_brace() { let c = '{'; let d = '}'; }";
    assert_eq!(class_of(src, "fn test_char_brace"), Class::Test);
}

#[test]
fn selftest_l22_fn_after_brace_char_literals_stays_prod() {
    let src = "#[cfg(test)]\nfn test_char_brace() { let c = '{'; let d = '}'; }\n\
               fn prod_after_char() {}";
    assert_eq!(class_of(src, "fn prod_after_char"), Class::Prod);
}

#[test]
fn selftest_l24_cfg_test_fn_with_a_brace_in_a_block_comment_is_test() {
    let src = "#[cfg(test)]\nfn test_block_comment() { /* { */ }";
    assert_eq!(class_of(src, "fn test_block_comment"), Class::Test);
}

#[test]
fn selftest_l25_fn_after_a_block_comment_brace_stays_prod() {
    let src = "#[cfg(test)]\nfn test_block_comment() { /* { */ }\nfn prod_after_block_comment() {}";
    assert_eq!(class_of(src, "fn prod_after_block_comment"), Class::Prod);
}

#[test]
fn selftest_l27_cfg_test_mod_declaration_is_test() {
    let src = "#[cfg(test)]\nmod tests;";
    let walked = walked(src);
    assert_eq!(walked.decls.len(), 1);
    assert_eq!(walked.decls[0].name, "tests");
    assert_eq!(walked.decls[0].class, Class::Test);
}

#[test]
fn selftest_l28_fn_after_a_cfg_test_mod_declaration_stays_prod() {
    let src = "#[cfg(test)]\nmod tests;\nfn prod_after_mod_decl() {}";
    assert_eq!(class_of(src, "fn prod_after_mod_decl"), Class::Prod);
}

#[test]
fn selftest_l30_cfg_test_if_else_statement_is_test() {
    let src = "fn host() {\n#[cfg(test)]\nif cond { a(); } else { b(); }\nprod_after_if_else();\n}";
    assert_eq!(call_class(src, "a"), Class::Test);
    assert_eq!(call_class(src, "b"), Class::Test);
}

#[test]
fn selftest_l31_statement_after_a_cfg_test_if_else_stays_prod() {
    let src = "fn host() {\n#[cfg(test)]\nif cond { a(); } else { b(); }\nprod_after_if_else();\n}";
    assert_eq!(call_class(src, "prod_after_if_else"), Class::Prod);
}

#[test]
fn selftest_l33_cfg_test_generic_fn_is_test() {
    let src = "#[cfg(test)]\nfn test_generic<A, B>(a: A, b: B) {}";
    assert_eq!(class_of(src, "fn test_generic"), Class::Test);
}

#[test]
fn selftest_l34_fn_after_a_generic_cfg_test_fn_stays_prod() {
    let src = "#[cfg(test)]\nfn test_generic<A, B>(a: A, b: B) {}\nfn prod_after_generic() {}";
    assert_eq!(class_of(src, "fn prod_after_generic"), Class::Prod);
}

#[test]
fn selftest_l36_tokio_test_async_fn_is_test() {
    assert_eq!(
        class_of("#[tokio::test]\nasync fn test_tokio() {}", "fn test_tokio"),
        Class::Test
    );
}

#[test]
fn selftest_l38_cfg_attr_never_gates() {
    let src = "#[cfg_attr(test, derive(Debug))]\nstruct ProdCfgAttr;";
    assert_eq!(class_of(src, "struct ProdCfgAttr"), Class::Prod);
}

#[test]
fn selftest_l40_cfg_test_inline_mod_and_its_contents_are_test() {
    let src = "#[cfg(test)]\nmod inline_tests { fn inner() {} }";
    assert_eq!(class_of(src, "mod inline_tests"), Class::Test);
    assert_eq!(class_of(src, "fn inner"), Class::Test);
}

#[test]
fn selftest_l41_fn_after_a_cfg_test_inline_mod_stays_prod() {
    let src = "#[cfg(test)]\nmod inline_tests { fn inner() {} }\nfn prod_after_inline_mod() {}";
    assert_eq!(class_of(src, "fn prod_after_inline_mod"), Class::Prod);
}

#[test]
fn selftest_l43_feature_fixtures_alone_is_fixtures() {
    let src = "#[cfg(feature = \"fixtures\")]\nfn fixtures_only() {}";
    assert_eq!(class_of(src, "fn fixtures_only"), Class::Fixtures);
}

#[test]
fn selftest_l45_a_feature_that_is_off_everywhere_is_test() {
    let src = "#[cfg(feature = \"other\")]\nfn test_other_feature_off() {}";
    assert_eq!(class_of(src, "fn test_other_feature_off"), Class::Test);
}

#[test]
fn selftest_l47_cfg_test_let_with_a_closure_is_test() {
    let src =
        "fn host() {\n#[cfg(test)]\nlet v = foo(|| { 1 });\n}\nfn prod_after_closure_stmt() {}";
    assert_eq!(class_of(src, "let v"), Class::Test);
    assert_eq!(call_class(src, "foo"), Class::Test);
}

#[test]
fn selftest_l48_fn_after_a_cfg_test_let_stays_prod() {
    let src =
        "fn host() {\n#[cfg(test)]\nlet v = foo(|| { 1 });\n}\nfn prod_after_closure_stmt() {}";
    assert_eq!(class_of(src, "fn prod_after_closure_stmt"), Class::Prod);
}

#[test]
fn selftest_l51_cfg_test_followed_by_another_attribute_is_test() {
    let src = "#[cfg(test)]\n#[allow(dead_code)]\nfn test_two_attrs() {}";
    assert_eq!(class_of(src, "fn test_two_attrs"), Class::Test);
}

#[test]
fn selftest_l52_fn_after_a_two_attribute_cfg_test_fn_stays_prod() {
    let src =
        "#[cfg(test)]\n#[allow(dead_code)]\nfn test_two_attrs() {}\nfn prod_after_two_attrs() {}";
    assert_eq!(class_of(src, "fn prod_after_two_attrs"), Class::Prod);
}

#[test]
fn selftest_l54_cfg_test_match_arm_is_test() {
    let src = "fn host(v: Foo) { match v { #[cfg(test)] Foo => { x() }, _ => prod_after_arm() } }";
    assert_eq!(call_class(src, "x"), Class::Test);
}

#[test]
fn selftest_l55_match_arm_after_a_cfg_test_arm_stays_prod() {
    let src = "fn host(v: Foo) { match v { #[cfg(test)] Foo => { x() }, _ => prod_after_arm() } }";
    assert_eq!(call_class(src, "prod_after_arm"), Class::Prod);
}

#[test]
fn selftest_l57_cfg_all_test_and_target_os_mod_declaration_is_test() {
    let src = "#[cfg(all(test, target_os = \"linux\"))]\nmod write_tests;";
    let walked = walked(src);
    assert_eq!(walked.decls[0].name, "write_tests");
    assert_eq!(walked.decls[0].class, Class::Test);
}

#[test]
fn selftest_l59_cfg_any_unknown_atom_or_test_fails_closed_to_prod() {
    let src = "#[cfg(any(unix, test))]\nfn prod_unknown_atom_fails_closed() {}";
    assert_eq!(
        class_of(src, "fn prod_unknown_atom_fails_closed"),
        Class::Prod
    );
}

#[test]
fn selftest_l61_cfg_test_impl_with_generics_and_a_comparison_is_test() {
    let src = "#[cfg(test)]\nimpl<T: Clone, U> Foo<T, U> { fn f(&self) -> bool { self.x >= 1 } }";
    assert_eq!(class_of(src, "impl"), Class::Test);
    assert_eq!(class_of(src, "fn f"), Class::Test);
}

#[test]
fn selftest_l62_fn_after_a_generic_cfg_test_impl_stays_prod() {
    let src = "#[cfg(test)]\nimpl<T: Clone, U> Foo<T, U> { fn f(&self) -> bool { self.x >= 1 } }\n\
               fn prod_after_impl() {}";
    assert_eq!(class_of(src, "fn prod_after_impl"), Class::Prod);
}

// ---------------------------------------------------------------------------------------------
// more classifier tests

#[test]
fn inner_cfg_test_attribute_gates_the_whole_file() {
    assert_eq!(walked("#![cfg(test)]\nfn f() {}").inner_class, Class::Test);
    assert_eq!(
        walked("#![cfg_attr(not(test), deny(warnings))]\nfn f() {}").inner_class,
        Class::Prod
    );
}

#[test]
fn test_attribute_with_arguments_is_test() {
    let src = "#[tokio::test(flavor = \"multi_thread\")]\nasync fn t() {}\n#[test]\nfn u() {}";
    assert_eq!(class_of(src, "fn t"), Class::Test);
    assert_eq!(class_of(src, "fn u"), Class::Test);
}

#[test]
fn a_test_item_inside_a_fixtures_item_and_a_fixtures_item_inside_a_test_item_are_test() {
    let src =
        "#[cfg(feature = \"fixtures\")]\nmod fx { #[cfg(test)] fn inner() {} fn plain() {} }\n\
               #[cfg(test)]\nmod t { #[cfg(feature = \"fixtures\")] fn inner2() {} }";
    assert_eq!(class_of(src, "fn inner"), Class::Test);
    assert_eq!(class_of(src, "fn plain"), Class::Fixtures);
    assert_eq!(class_of(src, "fn inner2"), Class::Test);
}

#[test]
fn not_any_test_or_fixtures_is_prod_and_all_not_test_fixtures_is_fixtures() {
    let src = "#[cfg(not(any(test, feature = \"fixtures\")))]\nfn a() {}\n\
               #[cfg(all(not(test), feature = \"fixtures\"))]\nfn b() {}\n\
               #[cfg(not(feature = \"fixtures\"))]\nfn c() {}";
    assert_eq!(class_of(src, "fn a"), Class::Prod);
    assert_eq!(class_of(src, "fn b"), Class::Fixtures);
    assert_eq!(class_of(src, "fn c"), Class::Prod);
}

#[test]
fn a_predicate_that_cannot_be_evaluated_is_prod() {
    let src = "#[cfg(foo bar baz)]\nfn a() {}\n#[cfg(not())]\nfn b() {}\n#[cfg = \"x\"]\nfn c() {}";
    assert_eq!(class_of(src, "fn a"), Class::Prod);
    assert_eq!(class_of(src, "fn b"), Class::Prod);
    assert_eq!(class_of(src, "fn c"), Class::Prod);
}

#[test]
fn a_feature_the_manifests_declare_is_unknown_not_off() {
    let src = "#[cfg(feature = \"tui\")]\nfn gated() {}\n#[cfg(feature = \"nobody-declares-this\")]\nfn dead() {}";
    let declared = CfgEnv {
        unknown_features: BTreeSet::from(["tui".to_string()]),
    };
    let strict = walk_file(&parse(src), &declared, Class::Prod, true).trace;
    let lookup = |trace: &[(String, Class)], label: &str| {
        trace
            .iter()
            .find(|(l, _)| l == label)
            .map(|(_, c)| *c)
            .unwrap()
    };
    // Declared in a manifest: a shipped build may enable it, so the item counts as production.
    assert_eq!(lookup(&strict, "fn gated"), Class::Prod);
    // Declared nowhere: no build can enable it.
    assert_eq!(lookup(&strict, "fn dead"), Class::Test);
    // census.py's reading (every feature but fixtures is off) hides the gated item.
    let lax = walked(src).trace;
    assert_eq!(lookup(&lax, "fn gated"), Class::Test);
}

// ---------------------------------------------------------------------------------------------
// tests of the call scanner

fn calls_of(src: &str) -> Vec<(String, String, Class, Form)> {
    walked(src)
        .calls
        .into_iter()
        .map(|c| (c.func, c.callee, c.class, c.form))
        .collect()
}

fn has_call(src: &str, func: &str, callee: &str) -> bool {
    calls_of(src)
        .iter()
        .any(|(f, c, _, _)| f == func && c == callee)
}

#[test]
fn a_call_is_attributed_to_its_innermost_fn_through_closures_async_and_nested_fns() {
    let src = "fn outer() {\n let c = || a.m1();\n let f = async { b.m2().await };\n\
               fn inner() { c.m3(); }\n}\nstatic S: u8 = k.m4();\nimpl T { fn method(&mut self) { self.m5(); } }";
    assert!(has_call(src, "outer", "m1"));
    assert!(has_call(src, "outer", "m2"));
    assert!(has_call(src, "inner", "m3"));
    assert!(has_call(src, OUTSIDE_FN, "m4"));
    assert!(has_call(src, "method", "m5"));
}

#[test]
fn macro_arguments_and_bodies_are_scanned_for_method_and_path_calls() {
    let src = "fn f() { assert!(a.mutate(1)); tokio::select! { v = b.other() => { c.third(); } } \
               m!(x.turbo::<Vec<u8>>(2)); n!(crate::funnel::append_and_project_audit(i)); }";
    assert!(has_call(src, "f", "mutate"));
    assert!(has_call(src, "f", "other"));
    assert!(has_call(src, "f", "third"));
    assert!(has_call(src, "f", "turbo"));
    let funnel = calls_of(src)
        .into_iter()
        .find(|(_, c, _, _)| c == "append_and_project_audit")
        .expect("path call inside a macro");
    assert_eq!(funnel.3, Form::Path { multi: true });
}

#[test]
fn range_dots_and_fn_definitions_inside_macros_are_not_calls() {
    let src =
        "fn f() { m!(0..a.len()); m!(0..=b.size()); }\nm!{ fn append_operation_audit(x: u8) {} }";
    assert!(has_call(src, "f", "len"));
    assert!(has_call(src, "f", "size"));
    assert!(!calls_of(src)
        .iter()
        .any(|(_, c, _, _)| c == "append_operation_audit"));
    assert!(!calls_of(src).iter().any(|(_, c, _, _)| c.is_empty()));
}

#[test]
fn a_call_in_a_cfg_test_block_is_test_and_in_a_cfg_not_test_block_is_prod() {
    let src = "fn f() {\n#[cfg(test)]\n{ a.gated(); }\n#[cfg(not(test))]\n{ b.live(); }\n}";
    assert_eq!(call_class(src, "gated"), Class::Test);
    assert_eq!(call_class(src, "live"), Class::Prod);
}

fn names(funnel: &[&str], mutator: &[&str]) -> Names {
    Names {
        funnel: funnel.iter().map(|s| s.to_string()).collect(),
        mutator: mutator.iter().map(|s| s.to_string()).collect(),
    }
}

fn findings_of(
    src: &str,
    file_class: Class,
    names: &Names,
) -> (BTreeMap<Key, Found>, BTreeMap<Key, Found>) {
    let (mut prod, mut fixtures) = (BTreeMap::new(), BTreeMap::new());
    findings(
        "x.rs",
        file_class,
        &walked(src).calls,
        names,
        &mut prod,
        &mut fixtures,
    );
    (prod, fixtures)
}

#[test]
fn a_mutator_is_a_method_call_or_a_pathed_call_but_never_a_bare_function() {
    let n = names(&[], &["mutate"]);
    let (prod, _) = findings_of(
        "fn f() { a.mutate(); Type::mutate(&mut a); mutate(a); }",
        Class::Prod,
        &n,
    );
    assert_eq!(prod.len(), 1, "one row per (file, fn, callee): {prod:?}");
    let (prod, _) = findings_of("fn f() { mutate(a); }", Class::Prod, &n);
    assert!(prod.is_empty());
    let (prod, _) = findings_of("fn f() { Type::mutate(&mut a); }", Class::Prod, &n);
    assert_eq!(prod.len(), 1);
}

#[test]
fn a_funnel_function_is_a_finding_bare_or_pathed() {
    let n = names(&["funnel"], &[]);
    for call in ["funnel(x)", "a::b::funnel(x)", "a.funnel(x)"] {
        let (prod, _) = findings_of(&format!("fn f() {{ {call}; }}"), Class::Prod, &n);
        assert_eq!(prod.len(), 1, "{call}");
    }
}

#[test]
fn fixtures_only_findings_are_reported_apart_and_test_only_ones_vanish() {
    let n = names(&[], &["mutate"]);
    let src =
        "fn live() { a.mutate(); }\n#[cfg(feature = \"fixtures\")]\nfn fx() { a.mutate(); }\n\
               #[cfg(test)]\nfn t() { a.mutate(); }";
    let (prod, fixtures) = findings_of(src, Class::Prod, &n);
    assert_eq!(
        prod.keys().map(|k| k.func.as_str()).collect::<Vec<_>>(),
        ["live"]
    );
    assert_eq!(
        fixtures.keys().map(|k| k.func.as_str()).collect::<Vec<_>>(),
        ["fx"]
    );
    let (prod, fixtures) = findings_of(src, Class::Test, &n);
    assert!(
        prod.is_empty() && fixtures.is_empty(),
        "a test-only file keeps nothing: {prod:?} {fixtures:?}"
    );
}

#[test]
fn method_sets_come_from_inherent_impls_with_a_mutable_receiver() {
    let src = "struct S;\nimpl S {\n pub fn a(&mut self) {}\n fn b(&mut self) {}\n pub fn c(&self) {}\n \
               pub fn d(mut self) {}\n pub fn e(self: &mut Self) {}\n pub fn g() {}\n}\n\
               impl Drop for S { fn drop(&mut self) {} }\n#[cfg(test)]\nimpl S { fn h(&mut self) {} }";
    let defs = walked(src).defs;
    let mutable: BTreeSet<&str> = defs
        .iter()
        .filter(|d| d.class != Class::Test && !d.trait_impl && d.mut_self)
        .map(|d| d.name.as_str())
        .collect();
    assert_eq!(mutable, BTreeSet::from(["a", "b", "e"]));
}

// ---------------------------------------------------------------------------------------------
// planted defects, in memory: each must fail the comparison with a clear message

const PLANT_ALLOWLIST: &str = "# comment\nx.rs\tlisted\tmutate\t1\tapplies-authority\tpermanent\n";

fn plant(src: &str, allowlist: &str) -> Vec<String> {
    let n = names(&["funnel"], &["mutate"]);
    let (prod, _) = findings_of(src, Class::Prod, &n);
    compare(
        &prod,
        &parse_allowlist(allowlist).expect("allowlist parses"),
    )
}

#[test]
fn baseline_listed_site_passes() {
    assert_eq!(
        plant("fn listed() { a.mutate(); }", PLANT_ALLOWLIST),
        Vec::<String>::new()
    );
}

#[test]
fn planted_unlisted_sql_mutator_call_in_a_production_fn_fails() {
    let problems = plant(
        "fn listed() { a.mutate(); }\nfn planted() { b.mutate(); }",
        PLANT_ALLOWLIST,
    );
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].starts_with("UNLISTED production call to the SQL-layer mutator"),
        "{problems:?}"
    );
    assert!(
        problems[0].contains("`mutate`") && problems[0].contains("fn `planted`"),
        "{problems:?}"
    );
}

#[test]
fn planted_legacy_funnel_call_fails() {
    let problems = plant(
        "fn listed() { a.mutate(); }\nfn planted() { funnel(x); }",
        PLANT_ALLOWLIST,
    );
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].starts_with("UNLISTED production call to the legacy audit funnel"),
        "{problems:?}"
    );
}

#[test]
fn planted_call_inside_a_cfg_not_test_block_fails() {
    let src = "fn listed() { a.mutate(); }\nfn planted() {\n#[cfg(not(test))]\n{ b.mutate(); }\n}";
    let problems = plant(src, PLANT_ALLOWLIST);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("fn `planted`"), "{problems:?}");
    // The same block under cfg(test) is not production and does not fail.
    let src = "fn listed() { a.mutate(); }\nfn planted() {\n#[cfg(test)]\n{ b.mutate(); }\n}";
    assert_eq!(plant(src, PLANT_ALLOWLIST), Vec::<String>::new());
}

#[test]
fn planted_unlisted_apply_frozen_call_fails() {
    let n = names(&["funnel"], &["mutate", "apply_frozen"]);
    let src = "fn listed() { a.mutate(); }\nfn planted() { index.apply_frozen(&record, path); }";
    let (prod, _) = findings_of(src, Class::Prod, &n);
    let allow = parse_allowlist(PLANT_ALLOWLIST).expect("allowlist parses");
    let problems = compare(&prod, &allow);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].starts_with("UNLISTED production call to the SQL-layer mutator"),
        "{problems:?}"
    );
    assert!(
        problems[0].contains("`apply_frozen`") && problems[0].contains("fn `planted`"),
        "{problems:?}"
    );
}

#[test]
fn planted_new_caller_of_the_funnel_entry_append_and_project_audit_locked_fails() {
    let n = names(&FUNNEL_FUNCTIONS, &["mutate"]);
    let src = "fn listed() { a.mutate(); }\nfn planted() { append_and_project_audit_locked(i, d, true, input); }";
    let (prod, _) = findings_of(src, Class::Prod, &n);
    let allow = parse_allowlist(PLANT_ALLOWLIST).expect("allowlist parses");
    let problems = compare(&prod, &allow);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].starts_with("UNLISTED production call to the legacy audit funnel"),
        "{problems:?}"
    );
    assert!(
        problems[0].contains("`append_and_project_audit_locked`")
            && problems[0].contains("fn `planted`"),
        "{problems:?}"
    );
}

#[test]
fn planted_second_call_to_an_allowed_callee_in_the_same_fn_fails() {
    let problems = plant("fn listed() { a.mutate(); b.mutate(); }", PLANT_ALLOWLIST);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].starts_with("COUNT MISMATCH") && problems[0].contains("2 time(s)"),
        "{problems:?}"
    );
    assert!(problems[0].contains("lists 1"), "{problems:?}");
    // A call inside a closure or a macro in the same fn is a call of that fn too.
    let problems = plant(
        "fn listed() { a.mutate(); m!(b.mutate()); }",
        PLANT_ALLOWLIST,
    );
    assert!(problems[0].starts_with("COUNT MISMATCH"), "{problems:?}");
}

#[test]
fn a_listed_count_above_the_code_fails_too() {
    let two = "# comment\nx.rs\tlisted\tmutate\t2\tapplies-authority\tpermanent\n";
    let problems = plant("fn listed() { a.mutate(); }", two);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].starts_with("COUNT MISMATCH") && problems[0].contains("A call was removed"),
        "{problems:?}"
    );
    assert_eq!(
        plant("fn listed() { a.mutate(); b.mutate(); }", two),
        Vec::<String>::new()
    );
}

#[test]
fn calls_are_counted_per_key_with_repeats() {
    let n = names(&[], &["mutate"]);
    let (prod, _) = findings_of(
        "fn f() { a.mutate(); a.mutate(); b.mutate(); }\nfn g() { a.mutate(); }",
        Class::Prod,
        &n,
    );
    let counts: Vec<(String, usize)> = prod
        .iter()
        .map(|(k, f)| (k.func.clone(), f.count))
        .collect();
    assert_eq!(counts, [("f".to_string(), 3), ("g".to_string(), 1)]);
}

#[test]
fn planted_stale_allowlist_row_fails() {
    let stale = format!("{PLANT_ALLOWLIST}x.rs\tgone\tmutate\t1\tcache\tpermanent\n");
    let problems = plant("fn listed() { a.mutate(); }", &stale);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].starts_with("STALE allowlist row"),
        "{problems:?}"
    );
    assert!(problems[0].contains("fn `gone`"), "{problems:?}");
}

// ---------------------------------------------------------------------------------------------
// the allowlist parser

#[test]
fn allowlist_rows_need_six_fields_a_count_a_known_reason_and_a_removal_order() {
    let ok = "a.rs\tf\tc\t1\tcache\tpermanent\n";
    assert!(parse_allowlist(ok).is_ok());
    for reason in REASONS {
        assert!(parse_allowlist(&format!("a.rs\tf\tc\t2\t{reason}\tL2\n")).is_ok());
    }
    assert!(parse_allowlist("a.rs\tf\tc\t1\tbecause\tpermanent\n")
        .unwrap_err()
        .contains("reason"));
    assert!(parse_allowlist("a.rs\tf\tc\t1\tcache\t \n")
        .unwrap_err()
        .contains("removed_by"));
    assert!(parse_allowlist("a.rs\tf\tc\tcache\tpermanent\n")
        .unwrap_err()
        .contains("6 tab-separated"));
    for bad in ["0", "-1", "two", ""] {
        assert!(
            parse_allowlist(&format!("a.rs\tf\tc\t{bad}\tcache\tpermanent\n"))
                .unwrap_err()
                .contains("count")
        );
    }
    assert!(parse_allowlist(&format!("{ok}{ok}"))
        .unwrap_err()
        .contains("duplicate"));
}

// ---------------------------------------------------------------------------------------------
// the module tree, on small workspaces written to a temporary directory

/// A workspace with one crate `c` whose manifest is `manifest`, and the given `c/...` files.
fn mini(manifest: &str, files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let write = |rel: &str, text: &str| {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    };
    write("Cargo.toml", "[workspace]\nmembers = [\"crates/c\"]\n");
    write(
        "crates/c/Cargo.toml",
        &format!("[package]\nname = \"c\"\n{manifest}"),
    );
    for (rel, text) in files {
        write(&format!("crates/c/{rel}"), text);
    }
    dir
}

fn tree_classes(dir: &tempfile::TempDir) -> (BTreeMap<String, Class>, Classes) {
    let ws = Workspace::load(dir.path(), &CfgEnv::default());
    let classes = classify_files(&ws.krates[0]);
    let by_name = classes
        .class
        .iter()
        .map(|(f, c)| (rel_string(f).replace("crates/c/", ""), *c))
        .collect();
    (by_name, classes)
}

#[test]
fn the_module_tree_follows_files_directories_inline_modules_and_path_attributes() {
    let dir = mini(
        "",
        &[
            (
                "src/lib.rs",
                "mod a;\nmod b;\n#[path = \"odd/x.rs\"]\nmod renamed;\nmod inline { mod deep; }\n",
            ),
            ("src/a.rs", "mod child;\n"),
            ("src/a/child.rs", ""),
            ("src/b/mod.rs", "mod sibling;\n"),
            ("src/b/sibling.rs", ""),
            ("src/odd/x.rs", "mod via_path;\n"),
            ("src/odd/via_path.rs", ""),
            ("src/inline/deep.rs", ""),
        ],
    );
    let (classes, c) = tree_classes(&dir);
    assert!(c.orphans.is_empty(), "{:?}", c.orphans);
    assert!(c.unresolved.is_empty(), "{:?}", c.unresolved);
    assert_eq!(classes.len(), 8);
    assert!(classes.values().all(|c| *c == Class::Prod));
}

#[test]
fn a_file_declared_under_cfg_test_is_test_and_so_are_its_children() {
    let dir = mini(
        "",
        &[
            (
                "src/lib.rs",
                "#[cfg(test)]\nmod tests;\n#[cfg(feature = \"fixtures\")]\nmod fx;\nmod live;\n",
            ),
            ("src/tests.rs", "mod nested;\n"),
            ("src/tests/nested.rs", ""),
            ("src/fx.rs", "mod inner;\n"),
            ("src/fx/inner.rs", ""),
            ("src/live.rs", ""),
        ],
    );
    let (classes, _) = tree_classes(&dir);
    assert_eq!(classes["src/lib.rs"], Class::Prod);
    assert_eq!(classes["src/live.rs"], Class::Prod);
    assert_eq!(classes["src/tests.rs"], Class::Test);
    assert_eq!(classes["src/tests/nested.rs"], Class::Test);
    assert_eq!(classes["src/fx.rs"], Class::Fixtures);
    assert_eq!(classes["src/fx/inner.rs"], Class::Fixtures);
}

#[test]
fn a_file_declared_twice_takes_the_least_test_like_declaration() {
    let dir = mini(
        "",
        &[(
            "src/lib.rs",
            "#[cfg(test)]\n#[path = \"shared.rs\"]\nmod as_test;\n#[cfg(not(test))]\n#[path = \"shared.rs\"]\nmod as_prod;\n",
        ), ("src/shared.rs", "")],
    );
    let (classes, _) = tree_classes(&dir);
    assert_eq!(classes["src/shared.rs"], Class::Prod);
}

#[test]
fn an_undeclared_file_is_an_orphan_and_counts_as_production() {
    let dir = mini("", &[("src/lib.rs", ""), ("src/stray.rs", "fn f() {}")]);
    let (classes, c) = tree_classes(&dir);
    assert_eq!(classes["src/stray.rs"], Class::Prod);
    assert_eq!(c.orphans.len(), 1);
}

#[test]
fn a_declared_module_with_no_file_is_unresolved_unless_it_is_test_only() {
    let dir = mini(
        "",
        &[("src/lib.rs", "mod missing;\n#[cfg(test)]\nmod tests;\n")],
    );
    let (_, c) = tree_classes(&dir);
    assert_eq!(c.unresolved.len(), 1, "{:?}", c.unresolved);
    assert!(c.unresolved[0].contains("mod missing;"));
}

#[test]
fn a_bin_that_requires_only_fixtures_is_fixtures_and_files_under_tests_are_not_read() {
    let dir = mini(
        "[[bin]]\nname = \"dev\"\npath = \"src/dev.rs\"\nrequired-features = [\"fixtures\"]\n",
        &[
            ("src/lib.rs", ""),
            ("src/dev.rs", "mod helper;\n"),
            ("src/helper.rs", ""),
            ("tests/it.rs", "fn f() {}"),
        ],
    );
    let (classes, c) = tree_classes(&dir);
    assert_eq!(classes["src/dev.rs"], Class::Fixtures);
    assert_eq!(classes["src/helper.rs"], Class::Fixtures);
    assert!(!classes.contains_key("tests/it.rs"));
    assert!(c.orphans.is_empty());
}

#[test]
#[should_panic(expected = "not a workspace member")]
fn a_crate_directory_missing_from_the_workspace_members_is_refused() {
    let dir = mini("", &[("src/lib.rs", "")]);
    fs::create_dir_all(dir.path().join("crates/other")).unwrap();
    fs::write(
        dir.path().join("crates/other/Cargo.toml"),
        "[package]\nname = \"other\"\n",
    )
    .unwrap();
    let _ = Workspace::load(dir.path(), &CfgEnv::default());
}
