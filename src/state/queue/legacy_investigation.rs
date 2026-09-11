//! Legacy Investigation parse-compatibility surface (issue #331,
//! DAR §12).
//!
//! Release N+1 removed the Investigation ACTIVE path (admission,
//! polling, worker prompts, finalization), but pre-N and N-era stores
//! may still carry `investigation` rows and N-era `worker-result.json`
//! files may still carry the `investigation` field. Deleting the
//! parse surface would hard-fail whole-store loads
//! (`FromSqlConversionFailure` in `row_to_entry`) or result parsing
//! (`deny_unknown_fields` on `WorkerResult`).
//!
//! REMOVAL CHECKLIST (issue #331, DAR §12):
//! This module exists so pre-N and N-era stores keep loading. It may be
//! deleted in the earliest release where no supported upgrade path from
//! a pre-N or N-era store remains (i.e. once direct pre-N→N+1 upgrades
//! are out of support). Deleting it requires deleting
//! `TicketType::Investigation`, the investigation `FinalizationStage`
//! variants, the `WorkerResult::investigation` field, and the
//! `PreviewReport::proposed_investigation_comment` field in the same
//! change. Until then: frozen pre-N fixture tests in CI exercise every
//! parse path (tests/state/investigation_removal_test.rs).
//!
//! What this module OWNS today:
//!
//! * the `ticket_type_to_string`/`ticket_type_from_str` serde
//!   round-trip that accepts `"investigation"` (defined inline in
//!   `store.rs` via serde `rename_all`; documented here),
//! * the `FinalizationStage` investigation string forms
//!   (`investigation_ready` / `investigation_commented`) retained in
//!   `queue::mod`'s `as_str`/`from_str` mappings (documented here),
//! * the `WorkerResult::investigation` serde default (documented on
//!   the field).
//!
//! The mappings themselves stay next to their types — extracting them
//! here would touch the persisted formats for zero behavior change.
//! This module is the named home and the checklist; the frozen
//! fixture tests are the CI exercise.

/// The ticket-type string a surviving pre-N / N-era row carries. Used
/// by the serde `rename_all = "snake_case"` mapping on
/// `TicketType` (parse side) and by `ticket_type_to_string` (persist
/// side). Never produced by N+1 admission paths.
pub const LEGACY_TICKET_TYPE_STRING: &str = "investigation";

/// FinalizationStage string forms retained for rows/checkpoints
/// written by release N. Never produced by N+1 finalization paths.
pub const LEGACY_STAGE_INVESTIGATION_READY: &str = "investigation_ready";
pub const LEGACY_STAGE_INVESTIGATION_COMMENTED: &str = "investigation_commented";
