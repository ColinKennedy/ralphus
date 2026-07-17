//! `#[ralphus_bench(patience = N)]` — proc-macro attribute for the RAL-94 Rust // N above is illustrative, not a chosen value — real call sites justify their own patience with a `// low: ...`-style comment
//! benchmark harness (see `ralphus-bench-harness`).
//!
//! Rust cannot achieve the same zero-annotation "universal inclusion" the
//! Python harness gets from a `pytest_pyfunc_call` hook: `cargo test`'s
//! implicit lib-unittest target (the one that runs `#[cfg(test)] mod tests`
//! inline in `src/`) cannot have its harness swapped out by an external
//! driver. So this crate implements the documented fallback instead —
//! explicit per-test opt-in — see the "RAL-94 benchmark harness" section of
//! `AGENTS.md` for the full rationale.
//!
//! Expansion is compile-time only and adds zero runtime cost to a normal
//! build: `#[ralphus_bench(patience = N)]` on `fn foo() { .. }` expands to
//! the original (unmodified) function, a separate `#[test]`-tagged wrapper
//! that calls it once (so it keeps running under ordinary `cargo test`), and
//! a `const` describing it — a `ralphus_bench_types::BenchMeta` — that an
//! explicit, hand-written `ralphus_bench_tests()` collector function
//! assembles into the list the `ralphus-bench-rs` binary drives through the
//! durable-minimum loop.

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::parse::Parser;
use syn::{ItemFn, parse_macro_input};

const DEFAULT_PATIENCE: u32 = 10;

#[proc_macro_attribute]
pub fn ralphus_bench(attr: TokenStream, item: TokenStream) -> TokenStream {
    let func = parse_macro_input!(item as ItemFn);

    if !func.sig.inputs.is_empty() {
        return syn::Error::new_spanned(
            &func.sig.inputs,
            "#[ralphus_bench] functions must take no arguments",
        )
        .to_compile_error()
        .into();
    }
    if !matches!(func.sig.output, syn::ReturnType::Default) {
        return syn::Error::new_spanned(
            &func.sig.output,
            "#[ralphus_bench] functions must return ()",
        )
        .to_compile_error()
        .into();
    }

    let patience = match parse_patience(attr) {
        Ok(patience) => patience,
        Err(err) => return err.to_compile_error().into(),
    };

    let fn_name = func.sig.ident.clone();
    let meta_name = format_ident!("{}_BENCH", fn_name.to_string().to_uppercase());
    // A distinctly-named wrapper carries `#[test]`, rather than putting it on
    // `#func` itself: rustc's `#[test]` attribute hides the annotated
    // function from ordinary name resolution outside `--test` builds, which
    // would break `run: #fn_name` below when this module is compiled
    // normally (i.e. whenever `ralphus-bench-rs` links this crate).
    let test_wrapper_name = format_ident!("{fn_name}__ralphus_bench_test");

    quote! {
        #func

        #[test]
        fn #test_wrapper_name() {
            #fn_name();
        }

        #[allow(dead_code)]
        pub(crate) const #meta_name: ::ralphus_bench_types::BenchMeta = ::ralphus_bench_types::BenchMeta {
            name: concat!(module_path!(), "::", stringify!(#fn_name)),
            file: file!(),
            patience: #patience,
            run: #fn_name,
        };
    }
    .into()
}

/// Parses an optional `patience = N` attribute argument; defaults to 10.
fn parse_patience(attr: TokenStream) -> syn::Result<u32> {
    if attr.is_empty() {
        return Ok(DEFAULT_PATIENCE);
    }

    let args = syn::punctuated::Punctuated::<syn::MetaNameValue, syn::Token![,]>::parse_terminated
        .parse(attr)?;

    for arg in &args {
        if arg.path.is_ident("patience") {
            return match &arg.value {
                syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Int(lit_int),
                    ..
                }) => lit_int.base10_parse::<u32>(),
                other => Err(syn::Error::new_spanned(
                    other,
                    "patience must be an integer literal",
                )),
            };
        }
    }

    Err(syn::Error::new(
        proc_macro2::Span::call_site(),
        "expected `patience = N`, e.g. #[ralphus_bench(patience = 3)]", // 3 here is illustrative, not a chosen value
    ))
}
