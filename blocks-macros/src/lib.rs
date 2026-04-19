//! Proc macros for [`ferrite_blocks`].
//!
//! Currently one attribute: [`ferrite_block`]. Applied to an `impl Block
//! for T { … }` item, it submits `T` into the static block registry so
//! `ferrite_blocks::registry::entries()` can enumerate known block types
//! without the registry being hand-maintained.
//!
//! The macro is deliberately thin: it preserves the impl verbatim and
//! only adds an `inventory::submit!` block alongside it. The registry
//! entry points at `<T as Block>::spec()` so the spec stays
//! single-sourced.
//!
//! ```ignore
//! use ferrite_blocks::{Block, BlockSpec};
//! use ferrite_blocks_macros::ferrite_block;
//!
//! pub struct MyBlock;
//!
//! #[ferrite_block]
//! impl Block for MyBlock {
//!     fn spec() -> BlockSpec { /* ... */ }
//!     // ...
//! }
//! ```

use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, ItemImpl, Type};

/// Attribute macro registering a block impl in the static registry.
///
/// See crate docs. Must be applied to an `impl Block for T` item; any
/// other shape is a compile error.
#[proc_macro_attribute]
pub fn ferrite_block(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as ItemImpl);

    let Some((_, trait_path, _)) = input.trait_.as_ref() else {
        return syn::Error::new_spanned(
            &input.self_ty,
            "#[ferrite_block] must be applied to an `impl Block for T` item",
        )
        .to_compile_error()
        .into();
    };

    let trait_last = trait_path
        .segments
        .last()
        .map(|s| s.ident.to_string())
        .unwrap_or_default();
    if trait_last != "Block" {
        return syn::Error::new_spanned(
            trait_path,
            "#[ferrite_block] must be applied to an impl of the `Block` trait",
        )
        .to_compile_error()
        .into();
    }

    let self_ty = &*input.self_ty;
    if !matches!(self_ty, Type::Path(_)) {
        return syn::Error::new_spanned(
            self_ty,
            "#[ferrite_block] needs a named Self type (e.g. `impl Block for MyBlock`)",
        )
        .to_compile_error()
        .into();
    }

    let expanded = quote! {
        #input

        ::ferrite_blocks::inventory::submit! {
            ::ferrite_blocks::registry::BlockEntry {
                spec_fn: || <#self_ty as ::ferrite_blocks::Block>::spec(),
                construct_fn: <#self_ty as ::ferrite_blocks::BlockFactory>::construct,
            }
        }
    };

    expanded.into()
}
