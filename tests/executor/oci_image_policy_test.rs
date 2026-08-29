use caduceus::executor::oci_image::{decide_pull_action, PullAction};
use caduceus::infra::config::OciPullPolicy;

#[test]
fn pull_policy_decision_table() {
    let cases = [
        (OciPullPolicy::Never, true, PullAction::UseLocal),
        (OciPullPolicy::Never, false, PullAction::ProbeLocalOnly),
        (OciPullPolicy::IfMissing, true, PullAction::UseLocal),
        (OciPullPolicy::IfMissing, false, PullAction::Pull),
        (OciPullPolicy::Always, true, PullAction::Pull),
        (OciPullPolicy::Always, false, PullAction::Pull),
    ];

    for (policy, local_present, expected) in cases {
        assert_eq!(
            decide_pull_action(policy, local_present),
            expected,
            "policy {policy:?}, local_present={local_present}"
        );
    }
}
