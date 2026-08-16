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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            model: "nova-3".into(),
            language: "en".into(),
            endpointing: 300,
            preserve_clipboard: false,
        }
    }
}

pub fn path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("rwhispr/config.toml")
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
    if cfg.api_key.is_empty() {
        cfg.api_key = std::env::var("DEEPGRAM_API_KEY").unwrap_or_default();
    }
    anyhow::ensure!(
        !cfg.api_key.is_empty(),
        "no Deepgram API key: set api_key in {} or $DEEPGRAM_API_KEY",
        p.display()
    );
    Ok(cfg)
}

pub fn socket_path() -> PathBuf {
    // ponytail: XDG_RUNTIME_DIR is set on every session that can run a
    // compositor; /tmp only matters for odd headless runs.
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(runtime).join("rwhispr.sock")
}
