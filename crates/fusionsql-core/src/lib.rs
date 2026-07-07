#![allow(clippy::must_use_candidate)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::return_self_not_must_use)]
#![allow(clippy::wildcard_imports)]
#![allow(clippy::redundant_closure_for_method_calls)]
#![allow(clippy::explicit_into_iter_loop)]

pub mod field;
pub mod filter;
pub mod id;
pub mod page;
#[cfg(feature = "with-sea-query")]
pub mod sea_utils;
pub mod utils;
