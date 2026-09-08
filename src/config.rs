use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub api_key: String,
    pub model: String,
    pub language: String,
    /// Deepgram endpointing in ms: silence before a segment is finalized.
    pub endpointing: u32,
    pub preserve_clipboard: bool,
    /// "deepgram" or "local" — which STT backend to use.
    pub provider: String,
    /// faster-whisper model for local provider, e.g. "tiny.en", "base.en", "small.en"
    pub local_model: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            model: "nova-3".into(),
            language: "en".into(),
            endpointing: 300,
            preserve_clipboard: false,
            provider: "local".into(),
            local_model: "tiny.en".into(),
        }
    }
}

pub fn path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("speakspic/config.toml")
}

pub fn load() -> Result<Config> {
    let p = path();
    let mut cfg: Config = match std::fs::read_to_string(&p) {
        Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", p.display()))?,
        Err(_) => {
            let cfg = Config::default();
            if let Some(d) = p.parent() {
                std::fs::create_dir_all(d)?;
            }
            std::fs::write(&p, toml::to_string_pretty(&cfg)?)?;
            eprintln!("wrote default config to {}", p.display());
            cfg
        }
    };
    // backwards compat: configs without provider default to local if no api_key
    if cfg.provider.is_empty() {
        cfg.provider = if cfg.api_key.is_empty() {
            "local".into()
        } else {
            "deepgram".into()
        };
    }
    if cfg.local_model.is_empty() {
        cfg.local_model = "tiny.en".into();
    }
    if cfg.provider == "deepgram" {
        if cfg.api_key.is_empty() {
            cfg.api_key = std::env::var("DEEPGRAM_API_KEY").unwrap_or_default();
        }
        anyhow::ensure!(
            !cfg.api_key.is_empty(),
            "provider is deepgram but no API key: set api_key in {} or $DEEPGRAM_API_KEY (or set provider = \"local\" for free local whisper)",
            p.display()
        );
    }
    Ok(cfg)
}

/// Load without requiring an API key — used to spawn the local worker.
// We don't want ensure_worker() to fail just because deepgram key is missing.
pub fn load_for_worker() -> Result<Config> {
    let p = path();
    let s = std::fs::read_to_string(&p)?;
    Ok(toml::from_str(&s)?)
}

pub fn socket_path() -> PathBuf {
    // ponytail: XDG_RUNTIME_DIR is set on every session that can run a
    // compositor; /tmp only matters for odd headless runs.
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(runtime).join("speakspic.sock")
}

pub fn local_socket_path() -> PathBuf {
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(runtime).join("speakspic-local.sock")
}
