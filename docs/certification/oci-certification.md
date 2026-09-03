# OCI live certification (issue #252)

This page is the certification report for the live adversarial Docker
suite and its CI gating model. It answers three questions: **what is
proven**, **on which engines/modes**, and **what the CI gates are**.
The checklist→test mapping below is asserted mechanically by
`tests/executor/certification_mapping_test.rs` (every mapped function
name must exist in the suite source), so the table cannot drift from
the code.

## What is proven

The suite drives **real containers through the real production path**
(`OciExecutor` → `sandbox_spec::resolve` → `sandbox_renderer::render`
→ `oci_lifecycle::run_oci_lifecycle`) and asserts worker-side failure
of hostile actions, or the daemon-side state that proves containment.
"Pure" = no engine needed (runs on every CI leg); "Live" = a real
Docker/Podman engine behind `CADUCEUS_RUN_ISOLATION_TESTS`.

| # | Checklist item (issue #252) | Test | Kind |
|---|---|---|---|
| 1 | Cannot read host sentinel outside allowed mounts | `oci_isolation_live_test.rs::host_sentinel_unreachable_live` | Live |
| 2 | Cannot access daemon state; cannot access other repositories | `oci_isolation_live_test.rs::daemon_state_and_other_repos_unreachable_live` | Live |
| 3 | `.git` does not reveal daemon Git metadata (pointer + directory shadows) | `oci_isolation_live_test.rs::git_shadow_read_sees_only_shadow` + `git_shadow_write_rejected` + `git_shadow_dir_variant_is_empty_and_read_only` | Live |
| 4 | Workspace writable; rootfs read-only; output/result path works | `oci_isolation_live_test.rs::workspace_writable_rootfs_readonly_output_writes_live` | Live |
| 5 | Writable mount surfaces == `{/workspace, /output}` + bounded `{/tmp, /dev/shm}` | `oci_isolation_live_test.rs::oci_mount_enumeration_two_writable_surfaces` | Live |
| 6 | Capabilities absent (cap-drop ALL from inside); no-new-privileges holds | `oci_isolation_live_test.rs::capabilities_absent_no_new_privileges_live` | Live |
| 7 | Runtime socket absent; device access unavailable | `oci_isolation_live_test.rs::runtime_socket_and_device_absent_live` | Live |
| 8 | Memory hog constrained | `oci_isolation_live_test.rs::memory_hog_oom_live` | Live |
| 9 | Fork bomb PID-constrained | `oci_isolation_live_test.rs::fork_bomb_eagain_live` | Live |
| 10 | CPU burn constrained | `oci_isolation_live_test.rs::cpu_burn_throttled_live` | Live |
| 11 | `/tmp` bounded | `oci_isolation_live_test.rs::tmpfs_bounded_live` | Live |
| 12 | `/dev/shm` bounded | `oci_isolation_live_test.rs::dev_shm_bounded_live` | Live |
| 13 | `network:none` cannot reach network | `oci_isolation_live_test.rs::network_none_unreachable_live` | Live |
| 14 | Unrestricted works AND is not host networking | `oci_isolation_live_test.rs::unrestricted_not_host_live` | Live |
| 15 | Daemon GitHub credentials absent | `oci_env_live_test.rs::oci_container_env_is_exact_canonical_plus_pass_env` | Live |
| 16 | Explicit `pass_env` variable present | `oci_env_live_test.rs::oci_container_env_is_exact_canonical_plus_pass_env` | Live |
| 17 | Unapproved environment absent | `oci_env_live_test.rs::oci_container_env_is_exact_canonical_plus_pass_env` | Live |
| 18 | Missing requested `pass_env` name aborts pre-create | `credential_leak_test.rs::missing_pass_env_aborts_pre_create` | Pure |
| 19 | Timeout cleans container | `oci_isolation_live_test.rs::timeout_cleans_container_live` | Live |
| 20 | Cancellation cleans container | `oci_isolation_live_test.rs::cancellation_cleans_container_live` | Live |
| 21 | Simulated daemon crash/restart reconciles the orphan | `oci_isolation_live_test.rs::crash_restart_orphan_reconcile_live` | Live |
| 22 | Heartbeat advances during a live run | `oci_isolation_live_test.rs::heartbeat_advances_during_run_live` (+ pure stub-engine `oci_lifecycle_stub_test.rs::heartbeat_refreshes_during_oci_wait_and_stops_after_resolution`) | Live (+Pure) |
| 23 | Wrong-digest image rejected before execution | `oci_isolation_live_test.rs::wrong_digest_rejected_before_execution_live` (+ pure `oci_image_verify_test.rs::digest_mismatch_is_distinct_and_checked_before_architecture`) | Live (+Pure) |
| 24 | Rootful identity correct | `oci_isolation_live_test.rs::rootful_docker_identity_canary` | Live |
| 25 | Rootless identity correct | `oci_isolation_live_test.rs::rootless_docker_identity_canary` | Live |
| 26 | Custom unrelated worker image succeeds (image neutrality) | `oci_isolation_live_test.rs::image_neutrality_custom_unrelated_image_live` | Live |

Checklist items 15–17 are covered by the env-transport live test
(`tests/integration/oci_env_live_test.rs`) through the exact
`OciEnvFile` path the executor uses; the dedicated
`credential_absence_live` / `pass_env_present_live` /
`unapproved_env_absent_live` cases from the plan are intentionally not
duplicated there (issue non-goal: no duplication without security
cause). The pure `missing_pass_env_aborts_pre_create` test proves the
frozen I9 semantics — resolution aborts with a typed error before any
`docker create`.

## Engines and modes

| Mode | Placement | What runs |
|---|---|---|
| Rootful Docker | **PR gate** — required check `ci / oci-live-certification` | Full suite (all 26 items) |
| Rootless Docker | Nightly (`oci-live-nightly.yml`, 07:30 UTC) + release certification | Full suite |
| Podman (Tier-2) | Nightly + release certification | Core sandbox contract subset (items 3, 4, 5, 6, 7, 19, 20, 24, 13, 14, 26, 1, 2 + env) |

The Podman leg forces the engine with `CADUCEUS_LIVE_TEST_ENGINE=podman`
(added to both live files' `detect_engine`), so it exercises Podman
even on runners that also have Docker. Per-mode identity canaries
skip-not-fail when the host engine does not match their mode.

## CI gating model

- **Merge gate (required):** `ci / oci-live-certification` in `ci.yml`.
  Path-filtered with `dorny/paths-filter@v3` to
  `src/executor/**`, `src/infra/config/**`, `src/state/oci_run*`,
  `src/daemon/tick/per_claim.rs`, `plugin-assets/worker-bridge.py`,
  `plugin-assets/worker-reference-image/**`, `tests/executor/**`,
  `tests/integration/oci_env_live_test.rs`, `.github/workflows/**`.
  Out-of-scope PRs self-skip to **success** (never `skipped`), so
  docs-only changes cannot block merge on a skipped required check.
  The job sets `CADUCEUS_RUN_ISOLATION_TESTS=1`, resolves a
  digest-pinned reference image (local registry push of the built
  reference image) plus a non-reference neutrality image (alpine),
  and runs `cargo nextest run --run-ignored all` over the two live
  binaries and the mapping self-check. Every live test runs against
  the reference image (`CADUCEUS_LIVE_TEST_IMAGE` is the shared
  default); only
  `image_neutrality_custom_unrelated_image_live` reads
  `CADUCEUS_LIVE_NEUTRALITY_IMAGE` and overrides its own sandbox
  image with it.
- **Nightly (not merge-gating):** `oci-live-nightly.yml` — rootless
  Docker and Podman Tier-2, 07:30 UTC daily.
- **Release:** `release.yml` runs all three modes in the
  `certify-oci` job; the release job `needs: certify-oci`.
- **Local:** the `#[ignore]` attributes are retained — without
  `CADUCEUS_RUN_ISOLATION_TESTS`, `cargo test` skips the live suite.
  The env var is the CI lift mechanism, not an opt-in local gate.

The existing shell-level `oci-reference-image` job in `ci.yml`
(image-contract gate: `docker run` smoke checks of the reference
image) is complementary and is **not** duplicated by this suite — the
Rust suite drives the production typed pipeline, the shell job proves
the image's own contract helpers.

## Failure diagnosability

- Container logs are captured to `<state_dir>/oci-runs/<run_id>/engine.log`
  by the lifecycle on every teardown path (`capture_engine_logs`), and
  the live tests assert this file exists on timeout/cancellation paths.
- Test assertions carry the container logs inline (every `assert_eq!`
  includes `logs: {logs}`), so a failure in the Actions log shows the
  worker output directly.
- The CI job uploads `oci-live-diagnostics` (junit XML, `engine.log`,
  `oci-runs/**/*.log`) as an artifact on failure, retained 14 days.

## Flake-budget / quarantine discipline

A flaky live test is **quarantined, never deleted**: add a tracking
issue, `#[ignore = "flaky: <issue>"]` the test, and keep the mapping
entry (the mapping test allows `#[ignore]`d entries). The quarantine
must not become permanent — the tracking issue stays open and the
release certification leg still runs the full suite (quarantine is a
PR-gate mitigation, not a coverage reduction).

## Residual risk (explicit)

- **Shared kernel:** containers share the host kernel. A kernel
  exploit from a worker is out of scope (issue non-goal). This suite
  proves **containment at the container/namespace/cgroup level**, not
  kernel isolation.
- **Given-to-worker data is readable by the worker:** the workspace
  mount, output mount, the env file (during `create`), and `pass_env`
  values are intentionally visible to the worker. The suite proves
  they do not **leak** to argv/logs/Debug/host-paths, not that they
  are hidden from the worker.
- **Rootless Docker on GitHub runners is fragile** (issue #252
  acknowledges runner support); it is a nightly + release leg. A
  rootless-only regression can land on `main` before the nightly
  catches it. Mitigation: the rootful PR gate covers the same
  invariants; rootless divergence is limited to identity/userns
  mapping.
- **Podman Tier-2 is nightly and a reduced set;** Podman-specific
  divergences outside the core contract are not caught until release
  certification.
- **Reproducibility:** `CADUCEUS_LIVE_TEST_IMAGE` and
  `CADUCEUS_LIVE_NEUTRALITY_IMAGE` are digest-pinned in every CI leg
  (local-registry push of the built reference image, and pulled
  alpine/busybox digests). A floating tag would make the suite
  non-reproducible and is rejected by the test fixtures.