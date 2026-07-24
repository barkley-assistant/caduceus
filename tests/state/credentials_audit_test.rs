//! captured evidence do not contain credential patterns.
//!
//! 3 tests: fixture scan, positive match, false-positive guard.
//!
//! Patterns scanned:
//! - `ghp_` — GitHub classic PAT
//! - `github_pat_` — GitHub fine-grained PAT
//! - `x-access-token:` — Git Actions token in URL
//! - `Basic ` — HTTP basic auth header

use std::fs;

// Credential pattern scanner

/// Returns `true` if `text` contains any known credential pattern.
fn has_credential_pattern(text: &str) -> bool {
    let patterns = ["ghp_", "github_pat_", "x-access-token:", "Basic "];
    patterns.iter().any(|p| text.contains(p))
}

// 1. Fixture scan — no credential patterns in test fixtures

#[test]
fn test_fixtures_contain_no_credentials() {
    let fixture_dir = {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        if !p.exists() {
            // Fallback: try relative to the test binary's working directory.
            let alt = std::path::Path::new("tests/fixtures");
            if alt.exists() {
                alt.to_path_buf()
            } else {
                panic!("fixtures directory not found at {:?} or {:?}", p, alt);
            }
        } else {
            p
        }
    };

    let mut scanned = 0usize;
    let mut violations: Vec<String> = Vec::new();

    for entry in fs::read_dir(&fixture_dir).expect("read fixtures dir") {
        let entry = entry.expect("entry");
        let path = entry.path();

        // Only scan Rust source files and YAML.
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !matches!(ext, "rs" | "yaml" | "py") {
            continue;
        }

        let content = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read fixture {}: {e}", path.display()));

        scanned += 1;

        if has_credential_pattern(&content) {
            violations.push(format!("{}", path.display()));
        }
    }

    assert!(
        scanned > 0,
        "at least one fixture file was scanned — found {scanned} files"
    );

    if !violations.is_empty() {
        panic!(
            "credential pattern found in {} fixture file(s):\n  {}",
            violations.len(),
            violations.join("\n  ")
        );
    }
}

// 2. Positive test — scanner matches known credential patterns

#[test]
fn credential_pattern_matches_known_credential() {
    // GitHub classic PAT
    assert!(
        has_credential_pattern("ghp_abc123def456ghi789jkl012mno345pqr678"),
        "ghp_ pattern must match classic PAT"
    );

    // GitHub fine-grained PAT
    assert!(
        has_credential_pattern("github_pat_11abc222def333ghi_444JKL567"),
        "github_pat_ pattern must match fine-grained PAT"
    );

    // Git Actions token in URL
    assert!(
        has_credential_pattern("https://x-access-token:ghp_token@github.com/owner/repo"),
        "x-access-token: pattern must match token in URL"
    );

    // HTTP basic auth header
    assert!(
        has_credential_pattern("Authorization: Basic "),
        "Basic  pattern must match auth header"
    );
}

// 3. Negative test — no false positives on safe strings

#[test]
fn credential_pattern_does_not_match_safe_strings() {
    let safe_strings = [
        // Commit SHAs (no credential prefix)
        "abc123def456",
        "a1b2c3d4e5f6g7h8i9",
        // Ordinary hex strings
        "0123456789abcdef",
        // "Basic" without trailing space / different casing
        "basic_example",
        "baseline_config",
        "not_basic",
        // JSON field names
        r#""token": "none""#,
        r#""auth_method": "basic""#,
        // Random identifiers
        "my_config_enabled",
        "scanning_for_credentials",
        "no_credentials_here",
        // URL fragments without token patterns
        "https://github.com/owner/repo",
        "https://api.github.com/repos/owner/repo",
        // Text mentioning the scanner (not matching any pattern)
        "checking for credential patterns",
        "credential audit scan completed",
        // Safe git references
        "refs/heads/main",
        "abc123def4567890",
    ];

    for s in &safe_strings {
        assert!(
            !has_credential_pattern(s),
            "safe string must NOT match: {s:?}"
        );
    }
}
