//! 阶段切换工具：LLM 进入每阶段时调用，报告当前阶段号。
//! agent 拦截此调用更新 session.phase，execute 只返回确认。

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use super::Tool;

pub struct SetPhase;

#[async_trait]
impl Tool for SetPhase {
    fn name(&self) -> &str {
        "set_phase"
    }
    fn description(&self) -> &str {
        "报告当前进入的流程阶段（0-5）。每进入一个新阶段时调用一次。阶段4整体调整时可回阶段3微调某 part。"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "phase": {
                    "type": "integer",
                    "description": "阶段编号 0-5：0种草闲聊（帮用户想好玩儿哪儿） 1信息采集 2大局规划（划分part+总览） 3逐part确定（景点+行程+住宿） 4整体调整+预算 5完整攻略",
                    "minimum": 0,
                    "maximum": 5
                }
            },
            "required": ["phase"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String> {
        let phase = args.get("phase").and_then(|v| v.as_u64()).unwrap_or(0);
        Ok(format!("已记录进入阶段 {phase}"))
    }
}
