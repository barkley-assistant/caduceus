//! Typed GitHub API surface. `Client`, repositories endpoint, issues
//! endpoint, and ETag-aware conditional GET are owned here.
//!
//! Every outbound mutation that posts a comment, a pull-request
//! title, or a pull-request body MUST route through
//! [`check_voice_or_error`] first. The validator is the single
//! entry point for the public-voice rule; nothing else in the
//! crate bypasses it.

#![allow(dead_code)]

// Submodule declarations and re-exports. These preserve the historical
// `crate::github::client` public surface byte-for-byte.

pub mod cache;
pub mod client_core;
pub mod http_helpers;
pub mod response;
pub mod voice;

pub use cache::*;
pub use client_core::*;
pub use http_helpers::*;
pub use response::*;
pub use voice::*;
