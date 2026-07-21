//! Tests for the upgrade choice validation.
//!
//! Verifies that [`UpgradePolicy::validate`] correctly handles the
//! tri-state upgrade decision.

use caduceus::executor::upgrade::{UpgradeChoice, UpgradePolicy};
use caduceus::executor::ExecutorKind;
use caduceus::infra::error::CaduceusError;

// ---------------------------------------------------------------------------
// missing_choice_rejected — None → OciUpgradeChoiceRequired
// ---------------------------------------------------------------------------

#[test]
fn missing_choice_rejected() {
    let result = UpgradePolicy::validate(None);
    match result {
        Err(CaduceusError::OciUpgradeChoiceRequired) => {} // expected
        Err(other) => panic!("expected OciUpgradeChoiceRequired; got: {other:?}"),
        Ok(()) => panic!("expected error for missing choice"),
    }
}

// ---------------------------------------------------------------------------
// not_asked_rejected — NotAsked → OciUpgradeChoiceRequired
// ---------------------------------------------------------------------------

#[test]
fn not_asked_rejected() {
    let result = UpgradePolicy::validate(Some(UpgradeChoice::NotAsked));
    match result {
        Err(CaduceusError::OciUpgradeChoiceRequired) => {} // expected
        Err(other) => panic!("expected OciUpgradeChoiceRequired; got: {other:?}"),
        Ok(()) => panic!("expected error for NotAsked"),
    }
}

// ---------------------------------------------------------------------------
// chosen_ok — Chosen(Oci) → Ok
// ---------------------------------------------------------------------------

#[test]
fn chosen_ok() {
    let result = UpgradePolicy::validate(Some(UpgradeChoice::Chosen(ExecutorKind::Oci)));
    assert!(
        result.is_ok(),
        "Chosen(Oci) should be valid, got: {result:?}"
    );
}

// ---------------------------------------------------------------------------
// chosen_trusted_host_ok — Chosen(TrustedHost) → Ok
// ---------------------------------------------------------------------------

#[test]
fn chosen_trusted_host_ok() {
    let result = UpgradePolicy::validate(Some(UpgradeChoice::Chosen(ExecutorKind::TrustedHost)));
    assert!(
        result.is_ok(),
        "Chosen(TrustedHost) should be valid, got: {result:?}"
    );
}

// ---------------------------------------------------------------------------
// deferred_ok — Deferred → Ok
// ---------------------------------------------------------------------------

#[test]
fn deferred_ok() {
    let result = UpgradePolicy::validate(Some(UpgradeChoice::Deferred));
    assert!(result.is_ok(), "Deferred should be valid, got: {result:?}");
}
