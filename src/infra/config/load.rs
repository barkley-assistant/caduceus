#![allow(dead_code, unused_imports)]
use super::*;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::infra::error::{CaduceusError, CaduceusResult};

// Resolution chain
// ---------------------------------------------------------------------------

/// Where the configuration came from. Used in error messages so the
/// operator can tell which level of the chain produced the failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResolvedSource {
    /// `$CADUCEUS_CONFIG` was set and pointed at this file.
    ExplicitEnv,
    /// `$HERMES_HOME/config.yaml` had a `caduceus:` section.
    HermesHome,
    /// `~/.config/caduceus/config.yaml` was the only one present.
    Standalone,
}

/// Compute the list of files to consider, in order, given the three
/// optional inputs from the loader.
pub(crate) fn resolve_sources(
    env: Option<&Path>,
    hermes: Option<&Path>,
    standalone: Option<&Path>,
) -> CaduceusResult<Vec<(ResolvedSource, std::path::PathBuf)>> {
    let mut sources: Vec<(ResolvedSource, std::path::PathBuf)> = Vec::new();
    if let Some(path) = env {
        let expanded = expand_leading_tilde(path.to_path_buf());
        sources.push((ResolvedSource::ExplicitEnv, expanded));
    }
    if let Some(hermes_home) = hermes {
        if hermes_home.as_os_str().is_empty() {
            return Err(CaduceusError::Config(
                "HERMES_HOME must not be empty".to_string(),
            ));
        }
        // Reject relative HERMES_HOME per the contract.
        if hermes_home.is_relative() {
            return Err(CaduceusError::Config(
                "HERMES_HOME must be an absolute path".to_string(),
            ));
        }
        sources.push((ResolvedSource::HermesHome, hermes_home.join("config.yaml")));
    }
    if let Some(path) = standalone {
        sources.push((ResolvedSource::Standalone, path.to_path_buf()));
    }
    Ok(sources)
}

/// Read the raw configuration from the first successful candidate.
/// Hermes files without a ``caduceus:`` section are skipped only when
/// a standalone source is also available.
pub(crate) fn load_raw_from_candidates(
    sources: &[(ResolvedSource, std::path::PathBuf)],
) -> CaduceusResult<RawConfig> {
    if sources.is_empty() {
        return Err(CaduceusError::Config(
            "no configuration source provided".to_string(),
        ));
    }

    // An explicit $CADUCEUS_CONFIG is an authoritative request — a
    // missing file is a hard error. The operator either meant for
    // that path to exist or set the variable by mistake.
    if let Some((ResolvedSource::ExplicitEnv, path)) = sources.first() {
        if !path.is_file() {
            return Err(CaduceusError::Config(format!(
                "$CADUCEUS_CONFIG points at {} but the file is missing",
                path.display()
            )));
        }
        return load_raw_from(path).map_err(|err| match err {
            CaduceusError::Yaml(yaml_err) => {
                CaduceusError::Config(format!("failed to parse {}: {yaml_err}", path.display()))
            }
            other => other,
        });
    }

    let mut standalone_seen = false;
    let mut hermes_seen_without_section = false;
    let mut last_missing_standalone: Option<&std::path::Path> = None;
    for (source, path) in sources {
        match source {
            ResolvedSource::HermesHome => {
                if !path.is_file() {
                    continue;
                }
                match load_raw_from(path) {
                    Ok(raw) => return Ok(raw),
                    Err(CaduceusError::Config(msg))
                        if msg.contains("missing 'caduceus:' section")
                            || msg.contains("has no 'caduceus:' section") =>
                    {
                        // Hermes file present but no caduceus section.
                        // Per the task, fall through only if a
                        // standalone source also exists.
                        hermes_seen_without_section = true;
                    }
                    Err(other) => return Err(other),
                }
            }
            ResolvedSource::Standalone => {
                standalone_seen = true;
                if !path.is_file() {
                    last_missing_standalone = Some(path.as_path());
                    continue;
                }
                // If a previous Hermes file was missing the section,
                // we still want the standalone file to take over.
                return load_raw_from(path);
            }
            ResolvedSource::ExplicitEnv => unreachable!(),
        }
    }

    if hermes_seen_without_section && !standalone_seen {
        return Err(CaduceusError::Config(
            "Hermes config has no 'caduceus:' section and no standalone config was found"
                .to_string(),
        ));
    }
    if hermes_seen_without_section {
        // Standalone was configured but the file was missing — surface
        // that explicitly so the operator can fix it.
        let path = last_missing_standalone
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<unset>".to_string());
        return Err(CaduceusError::Config(format!(
            "Hermes config has no 'caduceus:' section and standalone config {path} is missing"
        )));
    }

    Err(CaduceusError::Config(
        "no configuration source found (set $CADUCEUS_CONFIG, $HERMES_HOME, or write ~/.config/caduceus/config.yaml)".to_string(),
    ))
}

/// Read a single configuration file. Detects the Hermes shape (a top
/// level ``caduceus:`` mapping) and unwraps it before deserialising
/// into [`RawConfig`].
pub(crate) fn load_raw_from(path: &Path) -> CaduceusResult<RawConfig> {
    let text = std::fs::read_to_string(path).map_err(|err| {
        CaduceusError::Config(format!("failed to read {}: {err}", path.display()))
    })?;
    parse_raw_from_text(&text, path)
}

pub(crate) fn parse_raw_from_text(text: &str, source_path: &Path) -> CaduceusResult<RawConfig> {
    let outer: serde_yaml::Value = serde_yaml::from_str(text)?;
    let map = outer.as_mapping().ok_or_else(|| {
        CaduceusError::Config(format!(
            "expected a YAML mapping at the root of {}",
            source_path.display()
        ))
    })?;
    if map.contains_key("caduceus") {
        // Hermes-shaped file: extract the ``caduceus:`` mapping.
        let section = map.get("caduceus").ok_or_else(|| {
            CaduceusError::Config(format!(
                "missing 'caduceus:' section in {}",
                source_path.display()
            ))
        })?;
        let raw: RawConfig = serde_yaml::from_value(section.clone())?;
        return Ok(raw);
    }
    // Standalone-shaped file: every top-level key is part of the
    // raw config. We rely on ``deny_unknown_fields`` to catch
    // typos and stray sections — but only if the keys look like
    // Caduceus config. Detect Hermes-style keys (which the contract
    // expects on the same host) and treat the missing ``caduceus:``
    // section as an explicit error rather than a parse failure.
    for key in map.keys() {
        if let Some(name) = key.as_str() {
            if matches!(
                name,
                "model"
                    | "agent"
                    | "providers"
                    | "tools"
                    | "memory"
                    | "cron"
                    | "platforms"
                    | "gateway"
                    | "secrets"
                    | "voice"
                    | "mcp"
                    | "tts"
            ) {
                return Err(CaduceusError::Config(format!(
                    "Hermes config at {} has no 'caduceus:' section",
                    source_path.display()
                )));
            }
        }
    }
    let raw: RawConfig = serde_yaml::from_str(text)?;
    Ok(raw)
}

/// Apply ``CADUCEUS_DRY_RUN`` to a parsed Config.
pub(crate) fn apply_dry_run_env(cfg: &mut Config, value: &str) -> CaduceusResult<()> {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "1" | "true" | "yes" => cfg.dry_run = true,
        "0" | "false" | "no" => cfg.dry_run = false,
        _ => {
            return Err(CaduceusError::Config(format!(
                "CADUCEUS_DRY_RUN must be one of 1/true/yes/0/false/no (got {value:?})"
            )));
        }
    }
    Ok(())
}
