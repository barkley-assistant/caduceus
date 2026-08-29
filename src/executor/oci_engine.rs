//! Engine adapter for OCI image acquisition and inspection.
//!
//! Docker and Podman command differences intentionally stop at this module.
//! Callers consume [`NormalizedImage`] and do not need to know which CLI
//! produced it.

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::Value;

use crate::executor::sandbox_spec::SandboxEngine;
use crate::infra::error::{CaduceusError, CaduceusResult};

/// Engine-independent image identity and platform information.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NormalizedImage {
    pub id: String,
    pub repo_digests: Vec<String>,
    pub architecture: String,
    pub variant: Option<String>,
}

/// Lenient common subset of Docker and Podman inspect output.
///
/// The explicit aliases cover the casing emitted by current engines. The
/// value-based fallback in [`parse_inspect`] additionally accepts nested and
/// otherwise case-insensitive fields without making the normalizer depend on
/// one engine's exact JSON shape.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawInspect {
    #[serde(default, alias = "ID", alias = "id")]
    id: Option<String>,
    #[serde(
        default,
        alias = "REPO_DIGESTS",
        alias = "repo_digests",
        alias = "repoDigests",
        alias = "repodigests"
    )]
    repo_digests: Vec<String>,
    #[serde(default, alias = "ARCHITECTURE", alias = "architecture")]
    architecture: Option<String>,
    #[serde(default, alias = "VARIANT", alias = "variant")]
    variant: Option<String>,
}

/// Adapter that owns the Docker/Podman CLI contract.
#[derive(Clone, Debug)]
pub struct OciImageAdapter {
    engine: SandboxEngine,
    binary: PathBuf,
}

impl OciImageAdapter {
    /// Construct an adapter using the configured engine's normal binary name.
    pub fn new(engine: SandboxEngine) -> Self {
        Self {
            engine,
            binary: PathBuf::from(engine.binary_name()),
        }
    }

    /// Construct an adapter against an executable seam, primarily for
    /// offline integration tests.
    pub fn with_binary(engine: SandboxEngine, binary: impl Into<PathBuf>) -> Self {
        Self {
            engine,
            binary: binary.into(),
        }
    }

    /// Pull a digest-pinned image reference.
    pub async fn pull(&self, image_ref: &str) -> CaduceusResult<()> {
        let output = self
            .run_command(["pull", image_ref])
            .await
            .map_err(|detail| CaduceusError::OciPullFailed {
                image: image_ref.to_string(),
                stderr: detail,
            })?;
        if output.status.success() {
            Ok(())
        } else {
            Err(CaduceusError::OciPullFailed {
                image: image_ref.to_string(),
                stderr: command_stderr(&output),
            })
        }
    }

    /// Probe local image presence without contacting a registry.
    pub async fn image_exists(&self, image_ref: &str) -> CaduceusResult<bool> {
        let args = match self.engine {
            SandboxEngine::Docker => vec!["image", "inspect", image_ref],
            SandboxEngine::Podman => vec!["image", "exists", image_ref],
        };
        let output = self.run_command(args).await.map_err(|detail| {
            CaduceusError::OciImageInspectFailed {
                reference: image_ref.to_string(),
                detail,
            }
        })?;
        if output.status.success() {
            return Ok(true);
        }

        let stderr = command_stderr(&output);
        match self.engine {
            SandboxEngine::Docker if is_missing_image_message(&stderr) => Ok(false),
            SandboxEngine::Podman
                if output.status.code() == Some(1)
                    && (stderr.is_empty() || is_missing_image_message(&stderr)) =>
            {
                Ok(false)
            }
            _ => Err(CaduceusError::OciImageInspectFailed {
                reference: image_ref.to_string(),
                detail: stderr,
            }),
        }
    }

    /// Inspect an image and normalize the engine's JSON response.
    pub async fn inspect(&self, image_ref: &str) -> CaduceusResult<NormalizedImage> {
        let output = self
            .run_command(["image", "inspect", "--format", "{{json .}}", image_ref])
            .await
            .map_err(|detail| CaduceusError::OciImageInspectFailed {
                reference: image_ref.to_string(),
                detail,
            })?;
        if !output.status.success() {
            return Err(CaduceusError::OciImageInspectFailed {
                reference: image_ref.to_string(),
                detail: command_stderr(&output),
            });
        }
        parse_inspect_with_reference(&String::from_utf8_lossy(&output.stdout), image_ref)
    }

    async fn run_command<I, S>(&self, args: I) -> Result<std::process::Output, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let mut command = tokio::process::Command::new(&self.binary);
        command
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        command
            .output()
            .await
            .map_err(|err| format!("failed to execute {}: {err}", self.binary.display()))
    }
}

/// Parse Docker- or Podman-shaped inspect JSON into a common image shape.
pub fn parse_inspect(json: &str) -> CaduceusResult<NormalizedImage> {
    parse_inspect_with_reference(json, "<inspect>")
}

fn parse_inspect_with_reference(json: &str, reference: &str) -> CaduceusResult<NormalizedImage> {
    let value: Value = serde_json::from_str(json)
        .map_err(|err| inspect_error(reference, format!("invalid inspect JSON: {err}")))?;
    let object = value.as_object().ok_or_else(|| {
        inspect_error(
            reference,
            "inspect output must be a JSON object".to_string(),
        )
    })?;
    let raw: RawInspect = serde_json::from_value(value.clone())
        .map_err(|err| inspect_error(reference, format!("invalid inspect fields: {err}")))?;

    let id = raw
        .id
        .or_else(|| string_field(object, "id"))
        .ok_or_else(|| inspect_error(reference, "inspect output is missing Id".to_string()))?;
    let id = canonical_image_id(&id).ok_or_else(|| {
        inspect_error(
            reference,
            "Id must be sha256:<64 lowercase hex>".to_string(),
        )
    })?;

    let repo_digests = if raw.repo_digests.is_empty() {
        string_array_field(object, "repo_digests").unwrap_or_default()
    } else {
        raw.repo_digests
    };

    let config = object_field(object, "config");
    let architecture = raw
        .architecture
        .or_else(|| string_field(object, "architecture"))
        .or_else(|| {
            config
                .and_then(|v| v.as_object())
                .and_then(|v| string_field(v, "architecture"))
        })
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            inspect_error(
                reference,
                "inspect output is missing Architecture".to_string(),
            )
        })?;

    let variant = raw
        .variant
        .or_else(|| string_field(object, "variant"))
        .or_else(|| {
            config
                .and_then(|v| v.as_object())
                .and_then(|v| string_field(v, "variant"))
        })
        .and_then(|value| {
            let value = value.trim().to_ascii_lowercase();
            (!value.is_empty()).then_some(value)
        });

    Ok(NormalizedImage {
        id,
        repo_digests,
        architecture,
        variant,
    })
}

fn canonical_image_id(id: &str) -> Option<String> {
    let hex = id.strip_prefix("sha256:").unwrap_or(id);
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return None;
    }
    Some(format!("sha256:{hex}"))
}

fn object_field<'a>(object: &'a serde_json::Map<String, Value>, name: &str) -> Option<&'a Value> {
    object
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value)
}

fn string_field(object: &serde_json::Map<String, Value>, name: &str) -> Option<String> {
    object_field(object, name)?
        .as_str()
        .map(ToString::to_string)
}

fn string_array_field(object: &serde_json::Map<String, Value>, name: &str) -> Option<Vec<String>> {
    object_field(object, name)?.as_array().map(|items| {
        items
            .iter()
            .filter_map(|item| item.as_str().map(ToString::to_string))
            .collect()
    })
}

fn inspect_error(reference: &str, detail: String) -> CaduceusError {
    CaduceusError::OciImageInspectFailed {
        reference: reference.to_string(),
        detail,
    }
}

fn is_missing_image_message(stderr: &str) -> bool {
    let message = stderr.to_ascii_lowercase();
    message.contains("no such image")
        || message.contains("no such object")
        || message.contains("image not known")
        || message.contains("does not exist")
        || message.contains("not found")
}

fn command_stderr(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        format!("engine command exited with {}", output.status)
    } else {
        stderr
    }
}
