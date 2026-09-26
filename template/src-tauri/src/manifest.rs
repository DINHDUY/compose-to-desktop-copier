use std::collections::HashMap;
use std::path::Path;

use crate::compose::{ComposeError, DEFAULT_HEALTH_URL, DEFAULT_WAIT_SECS};

pub const MANIFEST_FILE: &str = "shell.toml";

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct ShellManifest {
    #[serde(default = "default_product_name")]
    pub product_name: String,
    #[serde(default = "default_project_name")]
    pub project_name: String,
    #[serde(default = "default_compose_file")]
    pub compose_file: String,
    #[serde(default = "default_health_url")]
    pub health_url: String,
    #[serde(default = "default_wait_secs")]
    pub wait_secs: u64,
    #[serde(default = "default_down_on_exit")]
    pub down_on_exit: bool,
    #[serde(default)]
    pub remove_volumes: bool,
    #[serde(default)]
    pub allow_remote_ui: bool,
    #[serde(default = "default_engine")]
    pub engine: String,
    #[serde(default)]
    pub secret_keys: Vec<String>,
}

fn default_product_name() -> String {
    "Desktop".to_string()
}

fn default_project_name() -> String {
    "compose-to-desktop-copier".to_string()
}

fn default_compose_file() -> String {
    "docker-compose.yml".to_string()
}

fn default_health_url() -> String {
    DEFAULT_HEALTH_URL.to_string()
}

fn default_wait_secs() -> u64 {
    DEFAULT_WAIT_SECS
}

fn default_engine() -> String {
    "auto".to_string()
}

fn default_down_on_exit() -> bool {
    true
}

impl Default for ShellManifest {
    fn default() -> Self {
        Self {
            product_name: default_product_name(),
            project_name: default_project_name(),
            compose_file: default_compose_file(),
            health_url: default_health_url(),
            wait_secs: default_wait_secs(),
            down_on_exit: true,
            remove_volumes: false,
            allow_remote_ui: false,
            engine: default_engine(),
            secret_keys: Vec::new(),
        }
    }
}

pub fn load(project_dir: &Path) -> Result<ShellManifest, ComposeError> {
    let path = project_dir.join(MANIFEST_FILE);
    let text = std::fs::read_to_string(&path).map_err(|err| {
        ComposeError::new(format!(
            "Could not read {} in {}: {err}",
            MANIFEST_FILE,
            project_dir.display()
        ))
    })?;
    toml::from_str(&text).map_err(|err| {
        ComposeError::new(format!(
            "{} in {} is invalid: {err}",
            MANIFEST_FILE,
            project_dir.display()
        ))
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedSettings {
    pub health_url: String,
    pub wait_secs: u64,
    pub window_url: String,
}

pub fn resolve_settings(
    manifest: &ShellManifest,
    project_dir: &Path,
) -> Result<ResolvedSettings, ComposeError> {
    let file_text = std::fs::read_to_string(project_dir.join(".env")).unwrap_or_default();
    let file_vars = parse_env_file(&file_text);
    let health_url = std::env::var("UI_HEALTH_URL")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| file_vars.get("UI_HEALTH_URL").cloned())
        .unwrap_or_else(|| manifest.health_url.clone());
    let wait_raw = std::env::var("UI_WAIT_SECS")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| file_vars.get("UI_WAIT_SECS").cloned());
    let wait_secs = match wait_raw {
        Some(value) => value.parse::<u64>().map_err(|_| {
            ComposeError::new(format!(
                "UI_WAIT_SECS must be a whole number of seconds, got {value}"
            ))
        })?,
        None => manifest.wait_secs,
    };
    if !manifest.allow_remote_ui {
        validate_loopback_host(&health_url)?;
    }
    let window_url = window_url_from_health(&health_url)?;
    Ok(ResolvedSettings {
        health_url,
        wait_secs,
        window_url,
    })
}

pub fn validate_loopback_host(health_url: &str) -> Result<(), ComposeError> {
    let url = url::Url::parse(health_url)
        .map_err(|err| ComposeError::new(format!("UI_HEALTH_URL is not a URL: {err}")))?;
    let host = url
        .host_str()
        .ok_or_else(|| ComposeError::new("UI_HEALTH_URL is missing a host"))?;
    if host != "127.0.0.1" && host != "localhost" {
        return Err(ComposeError::new(format!(
            "UI_HEALTH_URL must use 127.0.0.1 or localhost, got {host}. \
Set allow_remote_ui = true in shell.toml to override."
        )));
    }
    Ok(())
}

pub fn window_url_from_health(health_url: &str) -> Result<String, ComposeError> {
    let url = url::Url::parse(health_url)
        .map_err(|err| ComposeError::new(format!("UI_HEALTH_URL is not a URL: {err}")))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(ComposeError::new(
            "UI_HEALTH_URL must start with http:// or https://",
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| ComposeError::new("UI_HEALTH_URL is missing a host"))?;
    match url.port() {
        Some(port) => Ok(format!("{}://{host}:{port}", url.scheme())),
        None => Ok(format!("{}://{host}", url.scheme())),
    }
}

fn parse_env_file(text: &str) -> HashMap<String, String> {
    let mut vars = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        vars.insert(key.to_string(), unquote(value.trim()));
    }
    vars
}

fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_check_rejects_public_hosts() {
        let err = validate_loopback_host("http://example.com/").unwrap_err();
        assert!(err.to_string().contains("127.0.0.1"));
    }

    #[test]
    fn settings_prefer_env_over_manifest() {
        let dir =
            std::env::temp_dir().join(format!("compose-shell-manifest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(".env"),
            "UI_HEALTH_URL=http://127.0.0.1:3000/\nUI_WAIT_SECS=120\n",
        )
        .unwrap();
        let manifest = ShellManifest {
            health_url: "http://127.0.0.1:3000/".to_string(),
            wait_secs: 120,
            ..ShellManifest::default()
        };
        let previous = std::env::var("UI_HEALTH_URL").ok();
        let previous_wait = std::env::var("UI_WAIT_SECS").ok();
        std::env::set_var("UI_HEALTH_URL", "http://127.0.0.1:9/health");
        std::env::set_var("UI_WAIT_SECS", "3");
        let settings = resolve_settings(&manifest, &dir).unwrap();
        assert_eq!(settings.health_url, "http://127.0.0.1:9/health");
        assert_eq!(settings.wait_secs, 3);
        restore_env("UI_HEALTH_URL", previous);
        restore_env("UI_WAIT_SECS", previous_wait);
        std::fs::remove_dir_all(dir).ok();
    }

    fn restore_env(key: &str, value: Option<String>) {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }
}
