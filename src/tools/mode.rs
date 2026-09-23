//! 快捷模式子类型切换工具。
//!
//! 工具本身只负责校验输入；Agent 拦截成功调用并把结果写入当前 Session，
//! 这样下一次 LLM 请求就会使用新的提示词和工具白名单。

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde_json::Value;

use crate::session::QuickMode;

use super::Tool;

pub struct SetMode;

/// 从工具参数解析一个实际的快捷子模式。
pub fn parse_quick_mode(args: &Value) -> Result<QuickMode> {
    let raw = args
        .get("mode")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("set_mode 缺少 mode 参数"))?;
    let mode = match raw {
        "inspiration" => QuickMode::Inspiration,
        "schedule" => QuickMode::Schedule,
        "map" => QuickMode::Map,
        "xhs" => QuickMode::Xhs,
        "ctrip" => QuickMode::Ctrip,
        "knowledge" => QuickMode::Knowledge,
        "auto" | "full" => bail!("set_mode 不接受 {raw}，只能切换到实际快捷任务类型"),
        _ => bail!("set_mode 的 mode 无效：{raw}"),
    };
    Ok(mode)
}

#[async_trait]
impl Tool for SetMode {
    fn name(&self) -> &str {
        "set_mode"
    }

    fn description(&self) -> &str {
        "在快捷模式下选择任务类型：inspiration（种草）、schedule（排程）、map（地图）、xhs（小红书）、ctrip（携程候选核验）或 knowledge（知识库预留）。不要选择 auto/full。"
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "mode": {
                    "type": "string",
                    "enum": ["inspiration", "schedule", "map", "xhs", "ctrip", "knowledge"],
                    "description": "实际快捷任务类型，不是 auto 或 full"
                }
            },
            "required": ["mode"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let mode = parse_quick_mode(&args)?;
        let label = match mode {
            QuickMode::Inspiration => "inspiration",
            QuickMode::Schedule => "schedule",
            QuickMode::Map => "map",
            QuickMode::Xhs => "xhs",
            QuickMode::Ctrip => "ctrip",
            QuickMode::Knowledge => "knowledge",
            QuickMode::Auto => bail!("set_mode 不接受 auto"),
        };
        Ok(format!("已切换快捷任务类型为 {label}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_actual_quick_modes() {
        assert_eq!(
            parse_quick_mode(&serde_json::json!({"mode": "map"})).unwrap(),
            QuickMode::Map
        );
        assert!(parse_quick_mode(&serde_json::json!({"mode": "auto"})).is_err());
        assert!(parse_quick_mode(&serde_json::json!({"mode": "full"})).is_err());
        assert!(parse_quick_mode(&serde_json::json!({"mode": "bogus"})).is_err());
        assert!(parse_quick_mode(&serde_json::json!({})).is_err());
    }
}
