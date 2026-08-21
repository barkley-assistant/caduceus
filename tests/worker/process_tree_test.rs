use caduceus::worker_supervisor::TREE;

#[test]
fn own_process_has_no_children_at_test_start() {
    assert!(TREE.list_children(std::process::id() as i32).is_empty());
}

#[test]
fn adopting_own_process_is_idempotent() {
    TREE.adopt_subtree(std::process::id() as i32)
        .expect("first subtree adoption should be non-fatal");
    TREE.adopt_subtree(std::process::id() as i32)
        .expect("second subtree adoption should be non-fatal");
}
