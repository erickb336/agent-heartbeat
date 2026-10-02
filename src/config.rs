//! Configuration and private paths.
//!
//! Defaults need no config file: both sources, Seattle time, today's usage imported on the first run,
//! and no endpoint, so nothing is sent until one is configured.

use std::{
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{model::Provider, time::DEFAULT_TIME_ZONE};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    /// Where heartbeats go. HTTPS, or plain HTTP to this machine only (for the mock receiver).
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default = "default_time_zone")]
    pub time_zone: String,
    #[serde(default = "default_sources")]
    pub sources: Vec<Provider>,
    #[serde(default)]
    pub first_run: FirstRun,
    /// A file with the bearer secret, mode 0600. Without it, the secret comes from the Keychain.
    #[serde(default)]
    pub secret_file: Option<PathBuf>,
    #[serde(default = "default_active_interval")]
    pub active_interval_seconds: u64,
    #[serde(default = "default_idle_interval")]
    pub idle_interval_seconds: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FirstRun {
    /// Count today's usage from the existing files.
    #[default]
    ImportToday,
    /// Count only requests made after the first start.
    FromInstall,
}

fn default_time_zone() -> String {
    DEFAULT_TIME_ZONE.to_string()
}
fn default_sources() -> Vec<Provider> {
    vec![Provider::Claude, Provider::Codex]
}
fn default_active_interval() -> u64 {
    30
}
fn default_idle_interval() -> u64 {
    120
}

impl Default for Config {
    fn default() -> Self {
        serde_json::from_str("{}").unwrap()
    }
}

/// Every location the tool reads or writes. Environment variables move them, for tests and demos:
/// AGENT_HEARTBEAT_DIR (private state, config and logs), AGENT_HEARTBEAT_CLAUDE_HOME and
/// AGENT_HEARTBEAT_CODEX_HOME (the read-only sources).
#[derive(Clone, Debug)]
pub struct Paths {
    pub dir: PathBuf,
    pub logs: PathBuf,
    pub claude_home: PathBuf,
    pub codex_home: PathBuf,
}

impl Paths {
    pub fn from_env() -> Paths {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/".into()));
        let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
        let (dir, logs) = match var("AGENT_HEARTBEAT_DIR") {
            Some(dir) => (dir.clone(), dir.join("logs")),
            None => (home.join("Library/Application Support/agent-heartbeat"), home.join("Library/Logs/agent-heartbeat")),
        };
        Paths {
            dir,
            logs,
            claude_home: var("AGENT_HEARTBEAT_CLAUDE_HOME").unwrap_or_else(|| home.join(".claude")),
            codex_home: var("AGENT_HEARTBEAT_CODEX_HOME").unwrap_or_else(|| home.join(".codex")),
        }
    }

    pub fn config(&self) -> PathBuf {
        self.dir.join("config.json")
    }
    pub fn state(&self) -> PathBuf {
        self.dir.join("state.json")
    }
    pub fn last_send(&self) -> PathBuf {
        self.dir.join("last-send.json")
    }
    pub fn pid(&self) -> PathBuf {
        self.dir.join("agent-heartbeat.pid")
    }
    pub fn log(&self) -> PathBuf {
        self.logs.join("agent-heartbeat.log")
    }

    /// Creates the private folders with mode 0700.
    pub fn ensure_private_dirs(&self) -> std::io::Result<()> {
        for dir in [&self.dir, &self.logs] {
            fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

pub fn load(paths: &Paths) -> Result<Config, String> {
    let config = match fs::read(paths.config()) {
        Ok(bytes) => serde_json::from_slice::<Config>(&bytes).map_err(|e| format!("config.json: {e}"))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(e) => return Err(format!("config.json: {e}")),
    };
    if let Some(endpoint) = &config.endpoint {
        check_endpoint(endpoint)?;
    }
    if config.active_interval_seconds == 0 || config.idle_interval_seconds == 0 {
        return Err("config.json: intervals must be at least 1 second".into());
    }
    crate::time::Zone::new(&config.time_zone)?;
    Ok(config)
}

/// HTTPS anywhere, or plain HTTP to this machine only. No credentials or query in the URL.
pub fn check_endpoint(url: &str) -> Result<(), String> {
    let local = ["http://127.0.0.1:", "http://localhost:", "http://[::1]:"];
    if !(url.starts_with("https://") || local.iter().any(|p| url.starts_with(p))) {
        return Err("endpoint must use https:// (plain http is allowed for 127.0.0.1 only)".into());
    }
    let authority = url.split("://").nth(1).unwrap_or("").split('/').next().unwrap_or("");
    if authority.contains('@') || url.contains('?') || url.contains('#') {
        return Err("endpoint must not contain credentials, a query or a fragment".into());
    }
    Ok(())
}

/// Writes a file atomically with mode 0600: a temporary file, then a rename.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("tmp");
    let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_rules() {
        assert!(check_endpoint("https://erickbenitez.com/api/workstation/heartbeat").is_ok());
        assert!(check_endpoint("http://127.0.0.1:8787/api/workstation/heartbeat").is_ok());
        assert!(check_endpoint("http://erickbenitez.com/api").is_err());
        assert!(check_endpoint("https://user:pw@example.com/").is_err());
        assert!(check_endpoint("https://example.com/?token=x").is_err());
    }

    #[test]
    fn defaults_need_no_file() {
        let c = Config::default();
        assert_eq!(c.time_zone, "America/Los_Angeles");
        assert_eq!(c.sources, vec![Provider::Claude, Provider::Codex]);
        assert_eq!(c.first_run, FirstRun::ImportToday);
        assert!(c.endpoint.is_none());
    }
}
