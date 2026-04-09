use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Deserialize, PartialEq)]
pub struct BarConfig {
    pub width: u32,
}

#[derive(Debug, Deserialize)]
pub struct Config {
    pub config: BarConfig,
    pub layout: serde_json::Value,
}

pub fn default_config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".config/costae/config.yaml")
}

pub fn load_config(path: &Path) -> Result<Config, Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(path)?;
    Ok(serde_yaml::from_str(&content)?)
}
