//! 知识库枢纽工具（update_knowledge）：各类知识库的**统一写入口**（枢纽模式），
//! 避免每类知识库一个工具导致 LLM 选择负担与注册膨胀。
//!
//! 【接口预留，写入逻辑未实现】当前 execute 返回占位提示，不落任何文件；
//! 实现时按 `kind` 分派后端并放开 main.rs 的注册行（已留好注释锚点）：
//! - `location` → `skills/knowledge/locations/{province}.json`（景点搜索名+坐标，追加/去重）
//! - `guide`    → `skills/knowledge/guides/{province}.md`（攻略要点：预约/避坑/时长，追加小节）
//! - `hotel`    → 酒店评论沉淀（模式②：爬虫原始数据+个人评价写 markdown）
//!
//! 设计约定：写知识库必须走本枢纽（唯一入口），文件格式与去重规则代码写死，不让 LLM 自由发挥。

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::Value;

use super::Tool;

/// 已规划的枢纽后端（实现时逐个落地）。接口预留态暂无生产调用点。
#[allow(dead_code)]
pub const KINDS: &[&str] = &["location", "guide", "hotel"];

/// 枢纽工具本体。注册锚点在 main.rs（注释状态），实现写入后放开。
#[allow(dead_code)]
pub struct KnowledgeHub;

#[async_trait]
impl Tool for KnowledgeHub {
    fn name(&self) -> &str {
        "update_knowledge"
    }
    fn description(&self) -> &str {
        "【未启用】知识库写入枢纽：把可复用的确定性知识沉淀到项目知识库\
         （景点坐标 location / 攻略要点 guide / 酒店评价 hotel）。\
         当前为接口预留状态，调用不会落库。"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "kind": {"type": "string", "description": "知识库类型：location（景点坐标）| guide（攻略要点）| hotel（酒店评价沉淀）"},
                "province": {"type": "string", "description": "省份名（知识库按省分文件，如 吉林 / 陕西）"},
                "content": {"type": "string", "description": "要沉淀的内容（location 为 JSON 对象片段；guide/hotel 为 markdown）"}
            },
            "required": ["kind", "province", "content"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let kind = args
            .get("kind")
            .and_then(|v| v.as_str())
            .context("update_knowledge 缺少 kind 参数")?;
        if !KINDS.contains(&kind) {
            return Ok(format!(
                "知识库类型「{kind}」无效，可用：{}。本次内容未落库。",
                KINDS.join(" / ")
            ));
        }
        let province = args
            .get("province")
            .and_then(|v| v.as_str())
            .context("update_knowledge 缺少 province 参数")?;
        let has_content = args
            .get("content")
            .and_then(|v| v.as_str())
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        if !has_content {
            return Ok("content 为空，本次内容未落库。".to_string());
        }
        Ok(format!(
            "知识库枢纽接口已预留，{kind}/{province} 写入功能规划中（见 README 待做项「知识库」）。\
             本次内容未落库；请继续用 update_notes 记录到会话记忆，不影响当前规划。"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 占位行为：kind 校验、空内容校验、未落库提示。
    #[tokio::test]
    async fn hub_placeholder_behaviour() {
        let tool = KnowledgeHub;
        // 未知 kind → 列出可用类型
        let out = tool
            .execute(serde_json::json!({"kind": "foo", "province": "吉林", "content": "x"}))
            .await
            .unwrap();
        assert!(out.contains("无效") && out.contains("location"));
        // 空 content → 拒收
        let out = tool
            .execute(serde_json::json!({"kind": "guide", "province": "吉林", "content": "  "}))
            .await
            .unwrap();
        assert!(out.contains("content 为空"));
        // 正常路径 → 占位提示（不落库）
        let out = tool
            .execute(serde_json::json!({"kind": "location", "province": "吉林", "content": "{\"二道白河\": [128.1, 42.4]}"}))
            .await
            .unwrap();
        assert!(out.contains("未落库") && out.contains("location/吉林"));
        // 缺参数 → Err
        assert!(
            tool.execute(serde_json::json!({"kind": "guide"}))
                .await
                .is_err()
        );
    }
}
