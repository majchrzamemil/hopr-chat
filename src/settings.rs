use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// `(name, blokli url)`; the first is the default. hopr-lib 5 only talks to v5 networks.
pub const NETWORKS: [(&str, &str); 1] = [("piz-palu", "https://blokli-piz-palu.dev.hoprnet.link")];

pub fn blokli_url(network: &str) -> Option<String> {
    NETWORKS.iter().find(|(name, _)| *name == network).map(|(_, url)| url.to_string())
}

const VERSION: u32 = 1;

/// App settings persisted as `settings.toml` in the app home.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Settings {
    pub version: u32,
    pub nick: String,
    pub blokli_url: String,
    pub hops: u8,
    pub api_port: u16,
    pub p2p_port: u16,
    /// Explicit hoprd binary; otherwise the one bundled next to this executable, then PATH.
    pub hoprd_bin: Option<PathBuf>,
}

impl Settings {
    pub fn new(nick: String) -> Self {
        Self { version: VERSION, nick, blokli_url: NETWORKS[0].1.into(), hops: 1, api_port: 3100, p2p_port: 9100, hoprd_bin: None }
    }
}

/// `HOPR_CHAT_HOME`, else the per-OS application data directory.
pub fn home() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("HOPR_CHAT_HOME") {
        return Ok(dir.into());
    }
    let home = PathBuf::from(std::env::var("HOME").context("HOME not set")?);
    Ok(if cfg!(target_os = "macos") {
        home.join("Library/Application Support/hopr-chat")
    } else {
        std::env::var("XDG_DATA_HOME").map_or(home.join(".local/share"), PathBuf::from).join("hopr-chat")
    })
}

/// Data dir of a dev/attach chat: `~/.hopr-chat/<nick>`.
pub fn nick_dir(nick: &str) -> Result<PathBuf> {
    Ok(PathBuf::from(std::env::var("HOME").context("HOME not set")?).join(".hopr-chat").join(nick))
}

pub fn load(path: &Path) -> Result<Option<Settings>> {
    let Ok(text) = std::fs::read_to_string(path) else { return Ok(None) };
    let settings: Settings = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    if settings.version != VERSION {
        bail!("{} has version {}, this build understands {VERSION}", path.display(), settings.version);
    }
    Ok(Some(settings))
}

pub fn save(path: &Path, settings: &Settings) -> Result<()> {
    std::fs::write(path, toml::to_string_pretty(settings)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        assert!(load(&path).unwrap().is_none());
        let s = Settings::new("alice".into());
        save(&path, &s).unwrap();
        assert_eq!(load(&path).unwrap(), Some(s));
    }
}
