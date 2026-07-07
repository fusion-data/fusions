// region:    --- Modules
pub mod fusionsql_field;
pub mod struct_fusionsql_attr;

use quote::ToTokens;
use syn::{Attribute, DeriveInput, Expr, Field, FieldsNamed, Lit, MetaNameValue};

// endregion: --- Modules

/// Returns the `syn::` fields named of a struct.
///
/// 失败时返回 `syn::Error`（带 span），由 proc-macro 入口转成
/// `compile_error!` —— 让下游能看到出错位置，而不是宏内 panic。
pub(crate) fn get_struct_fields(ast: &DeriveInput) -> syn::Result<&FieldsNamed> {
  match &ast.data {
    syn::Data::Struct(syn::DataStruct { fields: syn::Fields::Named(fields), .. }) => Ok(fields),
    _ => Err(syn::Error::new_spanned(ast, "this derive macro only supports structs with named fields")),
  }
}

/// Returns the `type_name` of a field
pub(crate) fn get_type_name(field: &Field) -> String {
  format!("{}", &field.ty.to_token_stream())
}

pub fn get_field_attribute<'a>(field: &'a Field, name: &str) -> Option<&'a Attribute> {
  field.attrs.iter().find(|a| a.path().is_ident(name))
}

pub fn get_input_attribute<'a>(input: &'a DeriveInput, name: &str) -> Option<&'a Attribute> {
  input.attrs.iter().find(|a| a.path().is_ident(name))
}

pub fn get_meta_value_string(nv: MetaNameValue) -> Option<String> {
  if let Expr::Lit(exp_lit) = nv.value
    && let Lit::Str(lit_str) = exp_lit.lit
  {
    return Some(lit_str.value());
  }
  None
}
