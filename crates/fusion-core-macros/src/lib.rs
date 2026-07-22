#![allow(clippy::all)]
#![warn(clippy::exhaustive_structs)]

use proc_macro::TokenStream;
use syn::DeriveInput;

mod builder;
mod configuration;
mod inject;

/// Configuration
///
/// Generated code references `::fusions::core` by default; add
/// `#[fusions(crate = "::fusion_core")]` when depending on `fusion-core`
/// directly without the `fusions` umbrella crate.
#[proc_macro_derive(Configuration, attributes(config_prefix, fusions))]
pub fn derive_configuration(input: TokenStream) -> TokenStream {
  let input = syn::parse_macro_input!(input as DeriveInput);

  configuration::expand_derive(input).unwrap_or_else(syn::Error::into_compile_error).into()
}

/// Injectable Component
///
/// Generated code references `::fusions::core` by default; add
/// `#[fusions(crate = "::fusion_core")]` when depending on `fusion-core`
/// directly without the `fusions` umbrella crate.
#[proc_macro_derive(Component, attributes(config, component, fusions))]
pub fn derive_component(input: TokenStream) -> TokenStream {
  let input = syn::parse_macro_input!(input as DeriveInput);

  inject::expand_derive(input).unwrap_or_else(syn::Error::into_compile_error).into()
}

/// Builder
#[proc_macro_derive(Builder)]
pub fn derive_builder(item: TokenStream) -> TokenStream {
  builder::create_builder(item.into()).into()
}
