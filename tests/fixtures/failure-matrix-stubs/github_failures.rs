//! GitHub failure stub helpers for the failure matrix.
//!
//! Provides `ResponseTemplate` helpers for the failure scenarios
//! that Wiremock-backed GitHub endpoints need: 429 rate-limit,
//! 5xx server errors, and connection drops. Each helper returns a
//! `wiremock::ResponseTemplate` that the test mounts on the
//! appropriate matcher.

#![allow(dead_code)]

use wiremock::ResponseTemplate;

/// Build a 429 rate-limit response with the given reset-at
/// seconds (epoch) and the standard rate-limit headers.
pub fn rate_limit_429(reset_at: u64, remaining: u32, limit: u32) -> ResponseTemplate {
    ResponseTemplate::new(429)
        .insert_header("X-RateLimit-Reset", reset_at.to_string())
        .insert_header("X-RateLimit-Remaining", remaining.to_string())
        .insert_header("X-RateLimit-Limit", limit.to_string())
        .set_delay(std::time::Duration::from_millis(50))
}

/// Build a 500 server-error response with an optional delay.
pub fn server_error_500() -> ResponseTemplate {
    ResponseTemplate::new(500)
        .set_body_string("Internal Server Error")
        .set_delay(std::time::Duration::from_millis(50))
}

/// Build a 503 unavailable response.
pub fn unavailable_503() -> ResponseTemplate {
    ResponseTemplate::new(503)
        .set_body_string("Service Unavailable")
        .set_delay(std::time::Duration::from_millis(50))
}

/// A connection-drop simulation: return a 200 with an extremely
/// long delay so the client's timeout fires first.
pub fn connection_drop() -> ResponseTemplate {
    ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(60))
}
