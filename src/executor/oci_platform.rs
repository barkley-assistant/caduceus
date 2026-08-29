//! Host platform normalization for OCI image verification.

/// The OCI architecture and optional variant reported by the host build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostPlatform {
    pub architecture: String,
    pub variant: Option<String>,
}

/// Normalize Rust's target architecture name to the OCI architecture name.
///
/// Unknown target names are retained verbatim. The verifier deliberately
/// rejects those names rather than silently treating an unverifiable host as
/// compatible with an image.
pub fn host_platform() -> HostPlatform {
    let (architecture, variant) = match std::env::consts::ARCH {
        "x86_64" => ("amd64", None),
        "aarch64" => ("arm64", None),
        "armv7" => ("arm", Some("v7")),
        "powerpc64le" => ("ppc64le", None),
        "s390x" => ("s390x", None),
        "riscv64" => ("riscv64", None),
        "i686" => ("386", None),
        raw => (raw, None),
    };
    HostPlatform {
        architecture: architecture.to_string(),
        variant: variant.map(str::to_string),
    }
}
