//! Verify selected trigger label immediately before work. Task 2.5 owns
//! the body.
//!
//! The verifier re-fetches the issue via the typed client and decides
//! whether the worker may proceed. The function is the documented
//! "second pass" that catches the case where the issue was edited
//! between the original poll and the worker launch — the label may
//! have been removed, the issue may have been closed, or a 301
//! transfer to a different repository may have changed ownership.

use serde::Deserialize;

use crate::config::Config;
use crate::error::{CaduceusError, CaduceusResult};
use crate::github::{Client, ACCEPT_VALUE};
use crate::issue::IssueKey;
use crate::queue::TicketType;

/// Result of a single verification attempt.
///
/// - `Proceed` — the issue is still open, still labeled with the
///   expected trigger label, and the response URL matches the
///   requested key. The worker may launch.
/// - `Skip` — the issue no longer matches (closed, transferred,
///   label removed, or the object is a pull request). The
///   caller should release the claim and exit 0.
/// - `Err(_)` — a transient failure (auth, rate-limit, 5xx,
///   network, parse). The caller surfaces the error without
///   consuming a retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyOutcome {
    Proceed,
    Skip { reason: SkipReason },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The issue is closed (`state: "closed"`).
    Closed,
    /// The object is a pull request (the endpoint unifies PRs
    /// and issues).
    PullRequest,
    /// The trigger label is no longer on the issue.
    LabelRemoved,
    /// The issue transferred to a different repository; the
    /// response's `final_url` no longer matches the requested
    /// `owner/repo`.
    Transferred,
    /// The server returned 404 (deleted, made private, or the
    /// issue never existed).
    NotFound,
    /// The issue body is malformed in a way the verifier
    /// cannot recover from.
    Malformed,
}

impl SkipReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            SkipReason::Closed => "closed",
            SkipReason::PullRequest => "pull_request",
            SkipReason::LabelRemoved => "label_removed",
            SkipReason::Transferred => "transferred",
            SkipReason::NotFound => "not_found",
            SkipReason::Malformed => "malformed",
        }
    }
}

/// Verify the selected trigger label is still on the issue.
/// Returns `Ok(VerifyOutcome::Proceed)` when the worker may
/// proceed; `Ok(VerifyOutcome::Skip { .. })` when the caller
/// should release the claim and exit 0; `Err(_)` on transient
/// failures (auth, rate-limit, 5xx, network, parse).
///
/// The function selects the expected label from *config* by
/// *ticket_type* — `ticket_label_code` for `Code`,
/// `ticket_label_investigation` for `Investigation`. A
/// `BothLabels` case (the issue carries both trigger labels)
/// is a `Skip` because the issue is ambiguous per the polling
/// contract.
pub async fn verify_trigger(
    client: &Client,
    key: &IssueKey,
    ticket_type: TicketType,
    config: &Config,
) -> CaduceusResult<VerifyOutcome> {
    let expected_label = match ticket_type {
        TicketType::Code => &config.ticket_label_code,
        TicketType::Investigation => &config.ticket_label_investigation,
    };
    let path = format!("/repos/{}/{}/issues/{}", key.owner, key.repo, key.number);
    let response = match client.get(&path, ACCEPT_VALUE).await {
        Ok(r) => r,
        Err(CaduceusError::GitHubApi { status: 404, .. }) => {
            return Ok(VerifyOutcome::Skip {
                reason: SkipReason::NotFound,
            });
        }
        Err(err) => return Err(err),
    };
    // Auth / rate-limit / 5xx are all `Err` from `get_url`; the
    // 404 special case above is the only "skip" status. Anything
    // else propagates so the caller does not consume a retry.
    verify_response(
        response,
        key,
        expected_label,
        &config.ticket_label_code,
        &config.ticket_label_investigation,
    )
}

fn verify_response(
    response: crate::github::Response,
    key: &IssueKey,
    expected_label: &str,
    code_label: &str,
    investigation_label: &str,
) -> CaduceusResult<VerifyOutcome> {
    // 404 already handled by the caller.
    if response.status == 404 {
        return Ok(VerifyOutcome::Skip {
            reason: SkipReason::NotFound,
        });
    }
    if response.status != 200 && response.status != 201 {
        // Any other non-success is a transient error.
        return Err(CaduceusError::GitHubApi {
            status: response.status,
            message: String::from_utf8_lossy(&response.body).into_owned(),
        });
    }
    // Transfer detection: the response's final URL must still
    // point at the same owner/repo. The client follows up to
    // MAX_REDIRECTS same-origin hops; a cross-origin redirect is
    // an error and the request never succeeds. So the
    // "transferred" case here is "the issue was moved within
    // the same GitHub host", surfaced as a different
    // owner/repo in the final URL.
    if !response
        .final_url
        .contains(&format!("/{}/{}/", key.owner, key.repo))
    {
        return Ok(VerifyOutcome::Skip {
            reason: SkipReason::Transferred,
        });
    }
    let issue: IssueObject = serde_json::from_slice(&response.body).map_err(|err| {
        CaduceusError::Other(format!(
            "verify_trigger: JSON parse for {}/{}#{}: {err}",
            key.owner, key.repo, key.number
        ))
    })?;
    if issue.pull_request.is_some() {
        return Ok(VerifyOutcome::Skip {
            reason: SkipReason::PullRequest,
        });
    }
    let state = issue.state.as_deref().unwrap_or("open");
    if state.eq_ignore_ascii_case("closed") {
        return Ok(VerifyOutcome::Skip {
            reason: SkipReason::Closed,
        });
    }
    let labels: Vec<String> = issue
        .labels
        .unwrap_or_default()
        .into_iter()
        .map(|l| l.name.unwrap_or_default())
        .collect();
    let has_code = labels
        .iter()
        .any(|name| name.eq_ignore_ascii_case(code_label));
    let has_investigation = labels
        .iter()
        .any(|name| name.eq_ignore_ascii_case(investigation_label));
    if has_code && has_investigation {
        // Both labels on the same object — ambiguous per the
        // polling contract.
        return Ok(VerifyOutcome::Skip {
            reason: SkipReason::LabelRemoved,
        });
    }
    if !labels
        .iter()
        .any(|name| name.eq_ignore_ascii_case(expected_label))
    {
        return Ok(VerifyOutcome::Skip {
            reason: SkipReason::LabelRemoved,
        });
    }
    Ok(VerifyOutcome::Proceed)
}

/// One row of the GitHub `/repos/{owner}/{repo}/issues/{n}` payload.
/// The fields intentionally mirror the poll-side decoder; the
/// `state`, `labels`, and `pull_request` keys are the documented
/// verification path.
#[derive(Debug, Deserialize)]
struct IssueObject {
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    labels: Option<Vec<IssueLabel>>,
    /// Presence of this field signals the object is a pull
    /// request, not an issue.
    #[serde(default)]
    pull_request: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct IssueLabel {
    name: Option<String>,
}
