//! 功能开关：`.toggles.json`（gitignored，缺省全开）。
//! 管理 hotel 爬虫 / xhs 工具的启用与否，供 `cargo run -- hotel|xhs on|off` 使用。
//! 语义：开关与既有环境门槛相与——xhs 还需 config.toml [xhs].enabled，hotel 还需 env_ready。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const FILE: &str = ".toggles.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Toggles {
    #[serde(default = "default_true")]
    pub hotel_crawler: bool,
    #[serde(default = "default_true")]
    pub xhs: bool,
}

fn default_true() -> bool {
    true
}

impl Default for Toggles {
    fn default() -> Self {
        Self {
            hotel_crawler: true,
            xhs: true,
        }
    }
}

impl Toggles {
    pub fn load() -> Self {
        std::fs::read_to_string(FILE)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn set(key: &str, val: bool) -> Result<Self> {
        let mut t = Self::load();
        match key {
            "hotel" => t.hotel_crawler = val,
            "xhs" => t.xhs = val,
            other => anyhow::bail!("未知开关: {other}"),
        }
        std::fs::write(FILE, serde_json::to_string_pretty(&t)?)
            .with_context(|| format!("写入 {FILE} 失败"))?;
        Ok(t)
    }
}
