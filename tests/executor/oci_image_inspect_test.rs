use caduceus::executor::oci_engine::{parse_inspect, NormalizedImage};
use caduceus::infra::error::CaduceusError;

const DIGEST: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[test]
fn docker_and_podman_inspect_shapes_normalize_equivalently() {
    let docker = format!(
        r#"{{
            "Id": "sha256:{DIGEST}",
            "RepoDigests": ["registry.example/worker@sha256:{DIGEST}"],
            "Architecture": "AMD64",
            "Variant": null
        }}"#
    );
    let podman = format!(
        r#"{{
            "id": "{DIGEST}",
            "repo_digests": ["registry.example/worker@sha256:{DIGEST}"],
            "Config": {{"architecture": "amd64", "variant": ""}}
        }}"#
    );

    let expected = NormalizedImage {
        id: format!("sha256:{DIGEST}"),
        repo_digests: vec![format!("registry.example/worker@sha256:{DIGEST}")],
        architecture: "amd64".to_string(),
        variant: None,
    };
    assert_eq!(parse_inspect(&docker).expect("Docker fixture"), expected);
    assert_eq!(parse_inspect(&podman).expect("Podman fixture"), expected);
}

#[test]
fn inspect_parser_accepts_case_insensitive_top_level_fields() {
    let json = format!(
        r#"{{"iD":"sha256:{DIGEST}","rEpO_DiGeStS":["worker@sha256:{DIGEST}"],"aRcHiTeCtUrE":"amd64","vArIaNt":"v8"}}"#
    );
    let image = parse_inspect(&json).expect("case-insensitive fixture");
    assert_eq!(image.id, format!("sha256:{DIGEST}"));
    assert_eq!(image.repo_digests, vec![format!("worker@sha256:{DIGEST}")]);
    assert_eq!(image.architecture, "amd64");
    assert_eq!(image.variant.as_deref(), Some("v8"));
}

#[test]
fn malformed_inspect_and_invalid_ids_are_typed_inspect_failures() {
    for json in [
        "not json".to_string(),
        r#"{"Architecture":"amd64"}"#.to_string(),
        r#"{"Id":"sha256:bad","Architecture":"amd64"}"#.to_string(),
        format!(r#"{{"Id":"{DIGEST}"}}"#),
    ] {
        assert!(
            matches!(
                parse_inspect(&json),
                Err(CaduceusError::OciImageInspectFailed { .. })
            ),
            "expected typed inspect failure for {json}"
        );
    }
}
