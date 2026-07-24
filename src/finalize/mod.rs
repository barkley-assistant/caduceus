//! Finalization: commit, push, PR, comment/close, investigation comment.
//!
//! Idempotency across partial failures is the hard requirement — see
//! `CONTRACTS.md` "Finalization contract" and Tasks 6.1–6.5.
//!
//! This module owns the public-voice validator that every outbound
//! comment, PR title, and PR body must pass before the
//! corresponding API mutation. The validator lives in finalize.rs
//! because that is the only point through which GitHub mutations
//! flow; routing it through github.rs alone would leave a future
//! finalization caller free to bypass it.
//!
//! The public-voice rule is:
//!
//! * The text must not contain any `comment_forbidden_strings` term
//!   (case-insensitive Unicode substring match). Configuration
//!   replaces the defaults.
//! * The byte length must not exceed the documented limit for the
//!   channel (`limit` argument).
//!
//! On rejection the function returns the canonical [`VoiceError`]
//! (`Forbidden { found }` for substring matches, `TooLong { limit }`
//! for length). Both are terminal failures: the daemon's
//! retry-or-fail logic does not retry on a voice error.

// Submodule declarations and re-exports. These preserve the historical
// `crate::finalize` public surface.

pub mod comment_close;
pub mod commit;
pub mod dry_run_archive;
pub mod failure_investigation;
pub mod pr;
pub mod push;
pub mod reconcile;
pub mod voice;

pub use comment_close::*;
pub use commit::*;
pub use dry_run_archive::*;
pub use failure_investigation::*;
pub use pr::*;
pub use push::*;
pub use reconcile::*;
pub use voice::*;

pub use crate::finalize::voice::{
    terminal_from_voice, validate_comment, validate_pr_body, validate_pr_title,
    validate_public_text,
};
