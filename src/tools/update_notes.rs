//! 会话记忆笔记工具：LLM 调用 update_notes 整文件重写 sessions/{session_id}.md。
//! 当前笔记全文已注入 system prompt 尾部，LLM 每次看到全文再改，天然防丢。
//! agent 不拦截此调用，走正常 dispatch（写文件）。

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::Value;

use super::Tool;
use crate::session;

pub struct UpdateNotes {
    session_id: String,
}

/// 笔记硬性行数上限：超限拒绝写入，防记忆膨胀（总则建议 50 行内，硬限 60）。
const MAX_LINES: usize = 60;

impl UpdateNotes {
    pub fn new(session_id: String) -> Self {
        Self { session_id }
    }
}

#[async_trait]
impl Tool for UpdateNotes {
    fn name(&self) -> &str {
        "update_notes"
    }
    fn description(&self) -> &str {
        "更新会话记忆笔记（当前已确认信息的唯一事实源，全文附在系统提示词末尾）。\
         每次传入**完整**的 markdown 全文（整文件重写，不是增量）。\
         用户确认关键信息后、每阶段/每部分定稿后必须调用。内容要准确、精简。"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "笔记完整 markdown 全文；传空字符串表示清空"
                }
            },
            "required": ["content"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String> {
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .context("update_notes 缺少 content 参数")?;
        if content.trim().is_empty() {
            session::save_notes(&self.session_id, "").context("清空会话记忆失败")?;
            return Ok("已清空会话记忆".into());
        }
        // 硬性行数上限：不靠 LLM 自觉，超限直接拒收并要求精简
        let lines = content.lines().count();
        if lines > MAX_LINES {
            return Ok(format!(
                "写入失败：笔记共 {lines} 行，超过上限 {MAX_LINES} 行。\
                 请精简后重写：只保留用户已确认的关键事实，删除过程细节。"
            ));
        }
        session::save_notes(&self.session_id, content).context("保存会话记忆失败")?;
        Ok(format!("已更新会话记忆（{lines} 行），下一轮起生效"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 实跑：通过工具接口写读笔记（落到 sessions/test-notes.md，不联网）。
    #[tokio::test]
    async fn update_notes_roundtrip() {
        let tool = UpdateNotes::new("test-notes".into());
        let out = tool
            .execute(serde_json::json!({"content": "# 已确认信息\n- 2人\n- 10月出行"}))
            .await
            .expect("execute 失败");
        println!("{out}");
        assert!(out.contains("已更新会话记忆"));
        let notes = session::load_notes("test-notes").unwrap().unwrap();
        assert!(notes.contains("10月出行"));
        // 清理
        let _ = std::fs::remove_file("sessions/test-notes.md");
    }

    /// 缺 content 参数应报错而非 panic。
    #[tokio::test]
    async fn update_notes_missing_arg() {
        let tool = UpdateNotes::new("test-notes".into());
        assert!(tool.execute(serde_json::json!({})).await.is_err());
    }

    /// 超过行数上限应拒绝写入（返回失败文案，不落盘）。
    #[tokio::test]
    async fn update_notes_line_limit() {
        // 先落一个「旧版」笔记，验证超限写入不会破坏它
        let tool = UpdateNotes::new("test-notes-limit".into());
        tool.execute(serde_json::json!({"content": "# 旧版"}))
            .await
            .unwrap();
        let long: String = (0..MAX_LINES + 10)
            .map(|i| format!("第{i}行"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = tool
            .execute(serde_json::json!({ "content": long }))
            .await
            .unwrap();
        println!("{out}");
        assert!(out.contains("写入失败"), "超限应被拒收: {out}");
        assert_eq!(
            session::load_notes("test-notes-limit").unwrap().unwrap(),
            "# 旧版"
        );
        // 清理
        let _ = std::fs::remove_file("sessions/test-notes-limit.md");
    }
}
