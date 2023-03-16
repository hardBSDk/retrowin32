//! TODO: see TODO in main.rs.

use proc_macro2::TokenStream;
use quote::quote;
mod gen;

#[proc_macro_attribute]
pub fn dllexport(
    _attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let mut func: syn::ItemFn = syn::parse_macro_input!(item);
    let mut fmt: String = func.sig.ident.to_string();
    let mut args: Vec<&syn::Ident> = Vec::new();
    for arg in func.sig.inputs.iter().skip(1) {
        match arg {
            syn::FnArg::Typed(arg) => match &*arg.pat {
                syn::Pat::Ident(pat) => {
                    args.push(&pat.ident);
                }
                _ => {}
            },
            _ => {}
        };
    }
    fmt.push_str("(");
    fmt.push_str(
        &args
            .iter()
            .map(|arg| format!("{arg}:{{:x?}}"))
            .collect::<Vec<_>>()
            .join(", "),
    );
    fmt.push_str(")");
    let stmt = syn::parse_quote! {
        if TRACE { log::info!(#fmt, #(#args),*); }
    };
    func.block.stmts.insert(0, stmt);
    quote!(#func).into()
}

/// Generate a `shims` module that contains a wrapper for each function in this module
/// that transports arguments/return via the Machine's x86 stack.
#[proc_macro_attribute]
pub fn shims_from_x86(
    _attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let mut module: syn::ItemMod = syn::parse_macro_input!(item);

    // Generate one wrapper function per function found in the input module.
    let mut shims: Vec<TokenStream> = Vec::new();
    let items = &module.content.as_ref().unwrap().1;
    for item in items {
        match item {
            syn::Item::Fn(func) => {
                shims.push(gen::fn_wrapper(quote! { super }, func).into());
            }
            _ => {}
        }
    }

    // Generate a module containing the generated functions.
    let shims_module: syn::ItemMod = syn::parse2(quote! {
        pub mod shims {
            use super::*;
            use crate::winapi::stack_args::*;
            #(#shims)*
        }
    })
    .unwrap();

    // Add that module into the outer module.
    module
        .content
        .as_mut()
        .unwrap()
        .1
        .push(syn::Item::Mod(shims_module));
    let out = quote! {
        #module
    };
    // eprintln!("out {}", out);
    out.into()
}
