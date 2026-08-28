//! Doctor-report classification for the release-binary canary's
//! environmental skip-gate.
//!
//! The canary must skip (not fail) when `hermes caduceus doctor` cannot
//! run green because the HOST is not configured for caduceus — while
//! still failing loudly when the report shows a real daemon defect.
//!
//! Doctor's documented exit-code contract (pinned by
//! `tests/plugin/` doctor tests and implemented in the plugin's
//! `_cli_doctor`):
//!
//! * `0` — all checks healthy.
//! * `1` — config/runtime defects: `config-incomplete` (environmental,
//!   e.g. no provider secret in the shell) OR `daemon-defect` (a real
//!   defect: stale worktree locks, …). The two are distinguished only
//!   by the per-check `category:` line, which doctor prints under
//!   `--verbose`.
//! * `2` — host capability / external prerequisite
//!   (`host-capability-unavailable`, `gateway-inactive`).
//!
//! Output format being parsed (from `_cli_doctor`, verbose mode):
//!
//! ```text
//! [FAIL] Provider Secret — no provider secret name configured (checked …)
//!        next action: set one of CADUCEUS_GITHUB_TOKEN, …
//!        detail:      no provider secret name configured (checked …)
//!        category:    config-incomplete
//! ```
//!
//! Every `[FAIL]` line must resolve to a category; a FAIL without a
//! following `category:` line is treated as a defect (fail-safe
//! direction — format drift must never widen the skip gate).

/// Classification of one doctor run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DoctorVerdict {
    /// Exit 0 — every check healthy.
    Healthy,
    /// The host cannot support caduceus (or is not configured for it):
    /// exit 2, or exit 1 where every FAIL is `config-incomplete`.
    Skip(String),
    /// A real daemon defect, an unknown failure category, or an
    /// unparsable/empty report. The canary must fail.
    Defect(String),
}

/// Category assigned to a doctor finding that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailCategory {
    /// Environmental — operator configuration gap (no provider secret…).
    ConfigIncomplete,
    /// Real daemon defect (stale worktree lock, …).
    DaemonDefect,
    /// Any other category string; treated as a defect (fail-safe).
    Other,
}

impl FailCategory {
    fn from_str(raw: &str) -> Self {
        match raw.trim() {
            "config-incomplete" => FailCategory::ConfigIncomplete,
            "daemon-defect" => FailCategory::DaemonDefect,
            _ => FailCategory::Other,
        }
    }
}

/// Classify a doctor run from its exit code and verbose stdout.
///
/// `stdout` must be the combined report of
/// `hermes caduceus doctor --verbose`. Parsing is deliberately
/// line-based and lenient about indentation; it never panics.
pub fn classify_doctor(exit_code: i32, stdout: &str) -> DoctorVerdict {
    match exit_code {
        0 => {
            if stdout.trim().is_empty() {
                return DoctorVerdict::Defect(
                    "doctor exited 0 but produced no stdout report".to_string(),
                );
            }
            DoctorVerdict::Healthy
        }
        2 => DoctorVerdict::Skip(format!(
            "doctor exit 2 (host capability / external prerequisite): \
             first FAIL line: {:?}",
            first_fail_line(stdout).unwrap_or_default()
        )),
        1 => classify_exit_one(stdout),
        code => DoctorVerdict::Defect(format!(
            "doctor exited {code}, outside the documented 0/1/2 contract"
        )),
    }
}

/// Exit 1: skip only when EVERY failure is `config-incomplete`; fail on
/// any `daemon-defect`, unknown category, missing category, or empty
/// report.
fn classify_exit_one(stdout: &str) -> DoctorVerdict {
    if stdout.trim().is_empty() {
        return DoctorVerdict::Defect("doctor exited 1 but produced no stdout report".to_string());
    }
    let mut saw_fail = false;
    for category in fail_categories(stdout) {
        saw_fail = true;
        match category {
            FailCategory::ConfigIncomplete => continue,
            FailCategory::DaemonDefect => {
                return DoctorVerdict::Defect(format!(
                    "doctor reported a daemon-defect — not an \
                     environmental failure.\n{stdout}"
                ));
            }
            FailCategory::Other => {
                return DoctorVerdict::Defect(format!(
                    "doctor reported an unknown failure category — \
                     failing safe.\n{stdout}"
                ));
            }
        }
    }
    if !saw_fail {
        return DoctorVerdict::Defect(format!(
            "doctor exited 1 with no parsable [FAIL] categories — \
             failing safe.\n{stdout}"
        ));
    }
    DoctorVerdict::Skip(format!(
        "doctor exit 1 with only config-incomplete failures \
         (environmental): {}",
        first_fail_line(stdout).unwrap_or_default()
    ))
}

/// Extract the category for each `[FAIL]` finding in a verbose doctor
/// report. A `[FAIL]` with no following `category:` line yields
/// `FailCategory::Other` (fail-safe).
fn fail_categories(stdout: &str) -> Vec<FailCategory> {
    let mut categories = Vec::new();
    let mut pending_fail = false;
    for line in stdout.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("[FAIL]") {
            // Emit a placeholder for a previous FAIL that never got a
            // category line, then start tracking this one.
            if pending_fail {
                categories.push(FailCategory::Other);
            }
            pending_fail = true;
            continue;
        }
        if pending_fail {
            if let Some(raw) = trimmed.strip_prefix("category:") {
                categories.push(FailCategory::from_str(raw));
                pending_fail = false;
            } else if trimmed.starts_with('[') {
                // A new finding line arrived without a category.
                categories.push(FailCategory::Other);
                pending_fail = trimmed.starts_with("[FAIL]");
            }
        }
    }
    if pending_fail {
        categories.push(FailCategory::Other);
    }
    categories
}

/// First `[FAIL]` line, trimmed — for Skip/Defect reason messages.
fn first_fail_line(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .map(str::trim_start)
        .find(|l| l.starts_with("[FAIL]"))
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEALTHY: &str = "[OK] Binary — caduceus binary is built\n\
                           \n\
                           [OK] Provider Secret — provider secret name GITHUB_TOKEN is \
                           configured (no value read)\n";

    #[test]
    fn healthy_exit_zero_is_healthy() {
        assert_eq!(classify_doctor(0, HEALTHY), DoctorVerdict::Healthy);
    }

    #[test]
    fn exit_zero_with_empty_output_is_defect() {
        assert!(matches!(
            classify_doctor(0, "   \n"),
            DoctorVerdict::Defect(_)
        ));
    }

    #[test]
    fn exit_two_skips() {
        let report = "[FAIL] Cron Capability — the Hermes gateway is not \
                      reachable\n       category:    gateway-inactive\n";
        assert!(matches!(classify_doctor(2, report), DoctorVerdict::Skip(_)));
    }

    #[test]
    fn exit_one_config_incomplete_only_skips() {
        let report = "[OK] Binary — caduceus binary is built\n\
                      \n\
                      [FAIL] Provider Secret — no provider secret name \
                      configured (checked CADUCEUS_GITHUB_TOKEN, \
                      GITHUB_TOKEN, GH_TOKEN)\n\
                      \x20      next action: set one of CADUCEUS_GITHUB_TOKEN, \
                      GITHUB_TOKEN, or GH_TOKEN in the environment\n\
                      \x20      detail:      no provider secret name configured\n\
                      \x20      category:    config-incomplete\n";
        assert!(matches!(classify_doctor(1, report), DoctorVerdict::Skip(_)));
    }

    #[test]
    fn exit_one_with_daemon_defect_fails() {
        let report = "[FAIL] Worktree Lock — stale .worktrees/.lock at \
                      /x/.lock (no Caduceus daemon holds the flock)\n\
                      \x20      category:    daemon-defect\n\
                      \n\
                      [FAIL] Provider Secret — no provider secret name \
                      configured\n\
                      \x20      category:    config-incomplete\n";
        assert!(matches!(
            classify_doctor(1, report),
            DoctorVerdict::Defect(_)
        ));
    }

    #[test]
    fn exit_one_with_unknown_category_fails_safe() {
        let report = "[FAIL] Something — unexpected\n\
                      \x20      category:    quantum-anomaly\n";
        assert!(matches!(
            classify_doctor(1, report),
            DoctorVerdict::Defect(_)
        ));
    }

    #[test]
    fn exit_one_fail_without_category_line_fails_safe() {
        // Format drift: a FAIL that never prints a category line.
        let report = "[FAIL] Provider Secret — no provider secret name \
                      configured\n       next action: set one of …\n";
        assert!(matches!(
            classify_doctor(1, report),
            DoctorVerdict::Defect(_)
        ));
    }

    #[test]
    fn exit_one_empty_output_fails_safe() {
        assert!(matches!(classify_doctor(1, ""), DoctorVerdict::Defect(_)));
    }

    #[test]
    fn out_of_contract_exit_code_fails() {
        assert!(matches!(
            classify_doctor(127, "[FAIL] X — y\n       category:    config-incomplete\n"),
            DoctorVerdict::Defect(_)
        ));
    }

    #[test]
    fn multiple_consecutive_fails_each_get_a_category() {
        // Two [FAIL] lines back to back: the first never gets a
        // category before the next finding starts → Other (fail-safe);
        // the second resolves normally.
        let report = "[FAIL] A — first\n\
                      [FAIL] B — second\n\
                      \x20      category:    config-incomplete\n";
        assert_eq!(
            fail_categories(report),
            vec![FailCategory::Other, FailCategory::ConfigIncomplete]
        );
        assert!(matches!(
            classify_doctor(1, report),
            DoctorVerdict::Defect(_)
        ));
    }

    #[test]
    fn ok_lines_between_fails_do_not_swallow_categories() {
        let report = "[FAIL] A — first\n\
                      \x20      category:    config-incomplete\n\
                      \n\
                      [OK] Binary — fine\n\
                      \n\
                      [FAIL] B — second\n\
                      \x20      category:    config-incomplete\n";
        assert_eq!(
            fail_categories(report),
            vec![
                FailCategory::ConfigIncomplete,
                FailCategory::ConfigIncomplete
            ]
        );
        assert!(matches!(classify_doctor(1, report), DoctorVerdict::Skip(_)));
    }
}
