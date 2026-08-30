//! Core library for the super-docker terminal application.
//!
//! Keeping the application logic in a library gives tests and benchmarks one
//! canonical target.  The two installed binary names are intentionally thin
//! wrappers around this crate.

pub mod app;
pub mod compose;
pub mod docker;
pub mod http;
pub mod json;
pub mod operations;
pub mod ui;
pub mod update;
