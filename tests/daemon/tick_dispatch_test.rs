//! Tests for the tick dispatch site.
//!
//! Verifies that the dispatch in `src/daemon/tick.rs` calls
//! `services.executor.run(&spec)` and not the legacy
//! `services.process.supervise(...)` direct call. This is the
//! audit-grep companion to the trait-object dispatch test in
//! `executor_trusted_host_test`.

#[test]
fn tick_calls_executor_run_not_process_supervise() {
    let project_root = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    let tick_path = format!("{project_root}/src/daemon/tick.rs");
    let tick_src = std::fs::read_to_string(&tick_path)
        .unwrap_or_else(|e| panic!("cannot read {tick_path}: {e}"));

    // The dispatch must go through the trait object.
    assert!(
        tick_src.contains("services.executor.run"),
        "src/daemon/tick.rs must call services.executor.run; \
         the trait-object seam is the single dispatch point"
    );

    // The legacy direct call must be removed.
    assert!(
        !tick_src.contains("services.process.supervise"),
        "src/daemon/tick.rs must NOT call services.process.supervise; \
         the dispatch is owned by the executor trait object"
    );
}
