use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf, absolute};

use anyhow::{Context as _, Result, anyhow, bail};
use serde::Deserialize;
use warp_cli::agent::Harness;
use warp_core::paths::bundled_resources_dir;
use warp_util::path::file_exists_and_is_executable;

use super::super::super::AgentDriverError;
use super::launch::AcpLaunchSpec;

const SIDECAR_ENV: &str = "WARP_HARNESS_SIDECAR";
const MANIFEST_FILE: &str = "warp-harness.json";
const MANIFEST_DIR: &str = "${MANIFEST_DIR}";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    harness: String,
    acp: AcpLaunchSpec,
    install_docs_url: Option<String>,
    #[serde(default)]
    auth: Auth,
    #[serde(default)]
    env: HashMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Auth {
    check_command: Option<String>,
    #[serde(default)]
    runtime_error_patterns: Vec<String>,
}

pub(super) struct ResolvedManifest {
    pub(super) launch: AcpLaunchSpec,
    pub(super) install_docs_url: Option<String>,
    pub(super) auth_check_command: Option<String>,
    pub(super) runtime_error_patterns: Vec<String>,
    pub(super) env: HashMap<OsString, OsString>,
    pub(super) is_sidecar: bool,
}

pub(super) fn resolve(harness: Harness) -> Result<ResolvedManifest, AgentDriverError> {
    let override_dir = std::env::var_os(SIDECAR_ENV).map(PathBuf::from);
    discover(
        harness,
        override_dir.as_deref(),
        Path::new("/mnt"),
        bundled_resources_dir().as_deref(),
    )
    .map_err(|error| AgentDriverError::HarnessSetupFailed {
        harness: harness.to_string(),
        reason: format!("{error:#}"),
    })
}

fn discover(
    harness: Harness,
    override_dir: Option<&Path>,
    mounts_dir: &Path,
    resources_dir: Option<&Path>,
) -> Result<ResolvedManifest> {
    let mount_path = mounts_dir
        .join(format!("{harness}-cli-sidecar"))
        .join(MANIFEST_FILE);
    let bundle_path =
        resources_dir.map(|root| root.join("bundled/harnesses").join(format!("{harness}.json")));
    let (path, is_sidecar) = if let Some(dir) = override_dir {
        (dir.join(MANIFEST_FILE), true)
    } else if mount_path
        .try_exists()
        .with_context(|| format!("Cannot inspect {}", mount_path.display()))?
    {
        (mount_path, true)
    } else if let Some(path) = &bundle_path
        && path
            .try_exists()
            .with_context(|| format!("Cannot inspect {}", path.display()))?
    {
        (path.clone(), false)
    } else {
        let bundle_location = bundle_path
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| {
                format!("<bundled_resources_dir>/bundled/harnesses/{harness}.json (unavailable)")
            });
        bail!(
            "No ACP manifest for {harness}. Searched: ${SIDECAR_ENV}/{MANIFEST_FILE} \
             ({SIDECAR_ENV} unset), {}; {bundle_location}",
            mount_path.display()
        );
    };
    let dir = absolute(path.parent().context("Manifest has no parent directory")?)?;
    let mut manifest = read_manifest(&path, harness)?;
    if is_sidecar {
        manifest.acp.program = resolve_sidecar_program(&dir, &manifest.acp.program)
            .with_context(|| format!("Invalid ACP program in {}", path.display()))?;
    }
    let env = resolve_env(&manifest.env, &dir, std::env::vars_os().collect());
    log::info!("Selected ACP manifest for {harness}: {}", path.display());
    Ok(ResolvedManifest {
        launch: manifest.acp,
        install_docs_url: manifest.install_docs_url,
        auth_check_command: manifest.auth.check_command,
        runtime_error_patterns: manifest.auth.runtime_error_patterns,
        env,
        is_sidecar,
    })
}

fn read_manifest(path: &Path, harness: Harness) -> Result<Manifest> {
    let bytes = fs::read(path).with_context(|| format!("Cannot read manifest {}", path.display()))?;
    let manifest: Manifest = serde_json::from_slice(&bytes)
        .with_context(|| format!("Invalid manifest {}", path.display()))?;
    if manifest.schema_version != 1 {
        bail!(
            "Manifest {} uses unsupported schema_version {}; expected 1",
            path.display(),
            manifest.schema_version
        );
    }
    if manifest.harness != harness.to_string() {
        bail!(
            "Manifest {} declares harness {:?}; expected {harness}",
            path.display(),
            manifest.harness
        );
    }
    Ok(manifest)
}

fn resolve_sidecar_program(dir: &Path, program: &str) -> Result<String> {
    if program.is_empty() || program.contains(['/', '\\']) || matches!(program, "." | "..") {
        bail!("Sidecar ACP program must be a bare filename, got {program:?}");
    }
    let path = dir.join("usr/local/bin").join(program);
    let metadata = fs::metadata(&path)
        .with_context(|| format!("Sidecar ACP executable {} is missing or unreadable", path.display()))?;
    if !metadata.is_file() {
        bail!("Sidecar ACP executable {} is not a file", path.display());
    }
    if !file_exists_and_is_executable(&path) {
        bail!("Sidecar ACP executable {} is not executable", path.display());
    }
    path.into_os_string()
        .into_string()
        .map_err(|_| anyhow!("Sidecar ACP executable path is not valid UTF-8"))
}

fn resolve_env(
    env: &HashMap<String, String>,
    dir: &Path,
    process_env: HashMap<OsString, OsString>,
) -> HashMap<OsString, OsString> {
    env.iter()
        .map(|(name, value)| {
            let resolved = process_env.get(OsStr::new(name)).cloned().unwrap_or_else(|| {
                let mut resolved = OsString::new();
                let mut parts = value.split(MANIFEST_DIR);
                resolved.push(parts.next().unwrap_or_default());
                for part in parts {
                    resolved.push(dir);
                    resolved.push(part);
                }
                resolved
            });
            (OsString::from(name), resolved)
        })
        .collect()
}

#[cfg(test)]
#[path = "manifest_tests.rs"]
mod tests;
