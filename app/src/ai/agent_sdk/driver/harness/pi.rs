use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow};
use serde_json::{Map, Value, json};
use tempfile::{Builder, NamedTempFile, TempDir};
use warp_core::safe_info;
use warp_managed_secrets::ManagedSecretValue;

use super::JSONMCPServer;
use super::json_utils::{read_json_file_or_default, write_json_file};
use super::skill_dirs_publish::{HarnessSupportSkills, publish_skills_for_harness};
use crate::ai::ambient_agents::task::HarnessModelConfig;
use crate::ai::mcp::JSONTransportType;

const PI_PACKAGE_DIR_ENV: &str = "PI_PACKAGE_DIR";
const SIDECAR_PI_PACKAGE_DIR: &str = "/mnt/pi-cli-sidecar/usr/local/lib/pi-packages";
/// Pinned to the version baked into the sidecar image so native installs resolve the same
/// adapter the seeded extension tree carries.
const PI_MCP_ADAPTER_PACKAGE: &str = "npm:pi-mcp-adapter@5.2.0";
pub(super) const PI_CODING_AGENT_DIR_ENV: &str = "PI_CODING_AGENT_DIR";

/// Pi's installation assets root, when a run supplies one: a process-level `PI_PACKAGE_DIR`,
/// otherwise the sidecar mount if present. A native Pi install needs neither and locates its own
/// assets.
fn pi_package_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(PI_PACKAGE_DIR_ENV).filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let sidecar = PathBuf::from(SIDECAR_PI_PACKAGE_DIR);
    sidecar.is_dir().then_some(sidecar)
}

/// The pre-installed extension tree under a package dir, when the adapter was baked in ahead of
/// time so a run never needs to download it.
fn seeded_extension_dir(package_dir: &Path) -> Option<PathBuf> {
    let npm = package_dir.join("npm");
    npm.is_dir().then_some(npm)
}

pub(super) fn pi_env_vars() -> Result<(HashMap<OsString, OsString>, TempDir)> {
    let agent_dir = Builder::new().prefix("oz-pi-").tempdir()?;
    #[cfg(unix)]
    fs::set_permissions(agent_dir.path(), fs::Permissions::from_mode(0o700))?;
    let session_dir = agent_dir.path().join("sessions");
    fs::create_dir_all(&session_dir)?;
    let mut env_vars = HashMap::from([
        (
            OsString::from(PI_CODING_AGENT_DIR_ENV),
            agent_dir.path().as_os_str().to_owned(),
        ),
        (
            OsString::from("PI_CODING_AGENT_SESSION_DIR"),
            session_dir.into_os_string(),
        ),
        (OsString::from("PI_SKIP_VERSION_CHECK"), OsString::from("1")),
    ]);
    if let Some(package_dir) = pi_package_dir() {
        // Offline mode also stops Pi from installing missing packages, so it is only safe once
        // the adapter is known to be seeded from the package dir.
        if seeded_extension_dir(&package_dir).is_some() {
            env_vars.insert(OsString::from("PI_OFFLINE"), OsString::from("1"));
        }
        env_vars.insert(
            OsString::from(PI_PACKAGE_DIR_ENV),
            package_dir.into_os_string(),
        );
    }
    Ok((env_vars, agent_dir))
}

/// The startup notice pi-acp attaches to `session/new`, when it has one. pi-acp also streams
/// the same text as the session's first agent message chunk, ahead of any prompt, so callers
/// can recognize and drop that chunk. With `quietStartup` set this is only ever the "new Pi
/// version available" notice, which pi-acp produces by querying the npm registry on every new
/// session and offers no way to turn off.
pub(super) fn startup_message(session_meta: &Value) -> Option<String> {
    session_meta
        .pointer("/piAcp/startupInfo")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// Writes Pi's per-run configuration into its agent directory and returns that directory.
pub(super) fn prepare_pi_environment_config(
    harness_working_dir: &Path,
    system_prompt: Option<&str>,
    resolved_env_vars: &HashMap<OsString, OsString>,
    resolved_mcp_servers: &HashMap<String, JSONMCPServer>,
    resolved_secrets: &HashMap<String, ManagedSecretValue>,
    third_party_harness_model_config: Option<&HarnessModelConfig>,
) -> Result<PathBuf> {
    if let Some(config) = third_party_harness_model_config
        && config.model_id != "default"
        && !config
            .model_id
            .split_once('/')
            .is_some_and(|(provider, model)| valid_provider_id(provider) && !model.is_empty())
    {
        return Err(anyhow!("Pi models must use provider/model"));
    }
    let agent_dir = resolved_env_vars
        .get(OsStr::new(PI_CODING_AGENT_DIR_ENV))
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("{PI_CODING_AGENT_DIR_ENV} was not initialized"))?;
    // Pi installs extensions under its agent directory even when its installation assets are
    // read-only. Seeding the writable npm tree from a pre-installed one lets the pinned adapter
    // load without a download; otherwise Pi installs it from the registry on first launch.
    match resolved_env_vars
        .get(OsStr::new(PI_PACKAGE_DIR_ENV))
        .map(PathBuf::from)
        .and_then(|package_dir| seeded_extension_dir(&package_dir))
    {
        Some(seeded) => copy_writable_tree(&seeded, &agent_dir.join("npm"))
            .context("Failed to seed Pi adapter packages")?,
        None => log::info!("Pi will install {PI_MCP_ADAPTER_PACKAGE} on launch"),
    }
    prepare_pi_settings(&agent_dir.join("settings.json"))?;
    if let Some(prompt) = system_prompt {
        prepare_pi_system_prompt(&agent_dir, harness_working_dir, prompt)?;
    }
    write_json_file(
        &agent_dir.join("mcp.json"),
        &pi_mcp_config(resolved_mcp_servers),
        "Failed to serialize Pi MCP configuration",
    )?;
    prepare_pi_credentials(&agent_dir, resolved_env_vars, resolved_secrets)?;
    Ok(agent_dir)
}

fn valid_provider_id(provider: &str) -> bool {
    !provider.is_empty()
        && provider.len() <= 128
        && !matches!(provider, "__proto__" | "constructor" | "prototype")
        && provider.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
}

fn prepare_pi_credentials(
    agent_dir: &Path,
    resolved_env_vars: &HashMap<OsString, OsString>,
    resolved_secrets: &HashMap<String, ManagedSecretValue>,
) -> Result<()> {
    let mut credentials = HashMap::new();
    for secret in resolved_secrets.values() {
        if let ManagedSecretValue::PiProviderApiKey {
            provider_id,
            api_key,
            base_url,
        } = secret
        {
            if !valid_provider_id(provider_id) || api_key.is_empty() {
                return Err(anyhow!("Invalid Pi provider API key credential"));
            }
            let credential = (api_key.as_str(), base_url.as_deref());
            if credentials
                .insert(provider_id.as_str(), credential)
                .is_some_and(|previous| previous != credential)
            {
                return Err(anyhow!("Conflicting Pi credentials for the same provider"));
            }
        }
    }
    for secret in resolved_secrets.values() {
        let (provider, env_name, api_key, base_url) = match secret {
            ManagedSecretValue::AnthropicApiKey { api_key } => {
                ("anthropic", "ANTHROPIC_API_KEY", api_key, None)
            }
            ManagedSecretValue::OpenaiApiKey { api_key, base_url } => {
                ("openai", "OPENAI_API_KEY", api_key, base_url.as_deref())
            }
            _ => continue,
        };
        if credentials.contains_key(provider) {
            continue;
        }
        let active_key = std::env::var(env_name)
            .ok()
            .filter(|value| !value.is_empty())
            .or_else(|| {
                resolved_env_vars
                    .get(OsStr::new(env_name))
                    .and_then(|value| value.to_str())
                    .map(str::to_owned)
            });
        if active_key.as_deref() == Some(api_key.as_str()) {
            credentials.insert(provider, (api_key.as_str(), base_url));
        }
    }
    let mut auth = Map::new();
    let mut providers = Map::new();
    for (provider, (api_key, base_url)) in credentials {
        // Pi interprets !commands and $variables in keys; managed values must stay literal.
        let key = api_key.replace('$', "$$");
        let key = if let Some(command) = key.strip_prefix('!') {
            format!("$!{command}")
        } else {
            key
        };
        auth.insert(provider.to_owned(), json!({"type": "api_key", "key": key}));
        if let Some(base_url) = base_url {
            providers.insert(provider.to_owned(), json!({"baseUrl": base_url}));
        }
    }
    write_private_pi_json(agent_dir, "auth.json", &Value::Object(auth))?;
    if !providers.is_empty() {
        write_private_pi_json(agent_dir, "models.json", &json!({"providers": providers}))?;
    }
    Ok(())
}

fn write_private_pi_json(agent_dir: &Path, name: &str, value: &Value) -> Result<()> {
    let mut file =
        NamedTempFile::new_in(agent_dir).context("Failed to create Pi credential file")?;
    serde_json::to_writer(&mut file, value).context("Failed to serialize Pi credentials")?;
    file.flush().context("Failed to flush Pi credentials")?;
    file.persist(agent_dir.join(name))
        .map_err(|error| error.error)
        .context("Failed to persist Pi credentials")?;
    Ok(())
}

/// Publishes configured and bundled skills under `<agent_dir>/skills`, Pi's agent-level skill
/// root. The agent directory is private to the run, so nothing is written into the repository
/// and no git exclusion is needed.
pub(super) fn publish_skills_for_pi(
    workspace_root: &Path,
    agent_dir: &Path,
    skill_dirs: &[PathBuf],
    has_deferred_repositories: bool,
) -> Result<()> {
    let skill_root = agent_dir.join("skills");
    let published = publish_skills_for_harness(
        &skill_root,
        workspace_root,
        warp_isolation_platform::detect().is_some(),
        skill_dirs,
        has_deferred_repositories,
        HarnessSupportSkills::Bundled,
    )?;
    if !published.is_empty() {
        let published = published.len();
        safe_info!(
            safe: ("Published {published} skill(s) to the Pi skill root"),
            full: (
                "Published {published} skill(s) to Pi skill root {}",
                skill_root.display()
            )
        );
    }
    Ok(())
}

fn prepare_pi_settings(settings_path: &Path) -> Result<()> {
    let mut settings: Map<String, Value> = read_json_file_or_default(settings_path)?;
    settings.insert("defaultProjectTrust".to_owned(), json!("always"));
    settings.insert("quietStartup".to_owned(), json!(true));
    settings.insert("enableSkillCommands".to_owned(), json!(true));
    append_settings_entry(&mut settings, "packages", PI_MCP_ADAPTER_PACKAGE)?;
    append_settings_entry(&mut settings, "extensions", "-builtin:mcp")?;
    write_json_file(settings_path, &settings, "Failed to serialize Pi settings")
}

fn append_settings_entry(settings: &mut Map<String, Value>, key: &str, entry: &str) -> Result<()> {
    let entries = settings.entry(key.to_owned()).or_insert_with(|| json!([]));
    let entries = entries
        .as_array_mut()
        .ok_or_else(|| anyhow!("Pi settings {key} must be an array"))?;
    let value = json!(entry);
    if !entries.contains(&value) {
        entries.push(value);
    }
    Ok(())
}

fn prepare_pi_system_prompt(agent_dir: &Path, cwd: &Path, prompt: &str) -> Result<()> {
    // Project system prompts shadow the agent-level append slot, but AGENTS.md is concatenated.
    let slot = if cwd.join(".pi/APPEND_SYSTEM.md").exists() || cwd.join(".pi/SYSTEM.md").exists() {
        "AGENTS.md"
    } else {
        "APPEND_SYSTEM.md"
    };
    log::info!("Pi system prompt slot={slot}");
    fs::write(agent_dir.join(slot), prompt).context("Failed to write Pi system prompt")
}

fn pi_mcp_config(servers: &HashMap<String, JSONMCPServer>) -> Value {
    let servers: Map<String, Value> = servers
        .iter()
        .map(|(name, server)| {
            let config = match &server.transport_type {
                JSONTransportType::CLIServer {
                    command, args, env, ..
                } => {
                    json!({"command": command, "args": args, "env": env})
                }
                JSONTransportType::SSEServer { url, headers } => {
                    json!({"url": url, "headers": headers})
                }
            };
            (name.clone(), config)
        })
        .collect();
    json!({"mcpServers": servers})
}

fn copy_writable_tree(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        #[cfg(unix)]
        if entry.file_type()?.is_symlink() {
            symlink(fs::read_link(entry.path())?, &target)?;
            continue;
        }
        if entry.path().is_dir() {
            copy_writable_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
            #[cfg(unix)]
            {
                let permissions = fs::metadata(&target)?.permissions();
                fs::set_permissions(
                    &target,
                    fs::Permissions::from_mode(permissions.mode() | 0o200),
                )?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "pi_tests.rs"]
mod tests;
