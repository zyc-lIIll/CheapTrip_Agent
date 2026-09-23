use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::time::SystemTime;

use crate::llm::{Message, Usage};

/// Web 顶层工作流。旧会话缺少该字段时按完整攻略兼容读取。
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowMode {
    Quick,
    #[default]
    Full,
}

/// 快捷模式下由模型自动选择或由后续 set_mode 落地的任务类型。
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QuickMode {
    #[default]
    Auto,
    Inspiration,
    Schedule,
    Map,
    Xhs,
    Ctrip,
    Knowledge,
}

/// 新会话的开场白（作为 assistant 首条消息塞进 session，不靠 UI 硬编码）。
const GREETING: &str = "嗨～我是拾光者 🎒 你的专属旅游搭子！\n\
快捷模式：可以快速完成目的地种草、已有地点排程、地图、小红书总结、携程候选酒店核验等一次性任务。\n\
完整攻略：会从需求、路线、景点、住宿和交通开始，分阶段整理成最终攻略。\n\
个人知识库目前尚未启用。你想去哪儿玩，或者先把已有地点发给我？";

/// 一次会话：消息历史 + 累计用量 + 当前阶段 + 最后更新时间（R5：可保存/加载）。
#[derive(Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub usage: Usage,
    /// 当前所处阶段（0-5，0=种草闲聊），用于持久化/展示
    #[serde(default)]
    pub phase: u8,
    /// 顶层工作流；旧存档缺失时默认完整攻略。
    #[serde(default)]
    pub workflow_mode: WorkflowMode,
    /// 快捷模式子类型；完整攻略下保留最近选择但不生效。
    #[serde(default)]
    pub quick_mode: QuickMode,
    /// 最后一次对话的时间（排序/展示用，不受文件 mtime 影响）
    #[serde(default)]
    pub updated_at: Option<SystemTime>,
    /// 显示名（可中文，用户自定义）；None 时显示 id
    #[serde(default)]
    pub name: Option<String>,
    /// Web 账号所有者；认证关闭或 TUI 会话为 None。
    #[serde(default)]
    pub owner_user_id: Option<i64>,
}

impl Session {
    pub fn new(id: String) -> Self {
        Self::new_with_workflow_mode(id, WorkflowMode::Quick)
    }

    /// Create a session with the user's selected top-level workflow.
    /// `Session::new` remains the quick-mode default for CLI/TUI and old callers.
    pub fn new_with_workflow_mode(id: String, workflow_mode: WorkflowMode) -> Self {
        Self {
            id,
            messages: vec![Message {
                role: "assistant".into(),
                content: Some(GREETING.into()),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            }],
            usage: Usage::default(),
            phase: 0,
            workflow_mode,
            quick_mode: QuickMode::Auto,
            updated_at: Some(SystemTime::now()),
            name: None,
            owner_user_id: None,
        }
    }

    fn path(&self) -> std::path::PathBuf {
        let mut p = std::path::PathBuf::from("sessions");
        p.push(format!("{}.json", sanitize(&self.id)));
        p
    }

    pub fn save(&self) -> Result<()> {
        let path = self.path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("创建 sessions 目录失败")?;
        }
        let json = serde_json::to_string_pretty(self).context("序列化 session 失败")?;
        std::fs::write(&path, json).with_context(|| format!("写入 {} 失败", path.display()))?;
        Ok(())
    }

    pub fn load(id: &str) -> Result<Self> {
        let mut p = std::path::PathBuf::from("sessions");
        p.push(format!("{}.json", sanitize(id)));
        let json =
            std::fs::read_to_string(&p).with_context(|| format!("读取 {} 失败", p.display()))?;
        parse_session_json(&json).context("解析 session 失败")
    }

    /// 会话存档文件是否存在（Web 层区分 404 与损坏文件用）。
    pub fn exists(id: &str) -> bool {
        let mut p = std::path::PathBuf::from("sessions");
        p.push(format!("{}.json", sanitize(id)));
        p.exists()
    }

    /// 删除指定 id 的会话文件 + 联动删除该会话的地图图片目录与记忆笔记。
    pub fn delete(id: &str) -> Result<()> {
        let sid = sanitize(id);
        let mut p = std::path::PathBuf::from("sessions");
        p.push(format!("{sid}.json"));
        if p.exists() {
            std::fs::remove_file(&p).with_context(|| format!("删除 {} 失败", p.display()))?;
        }
        // 联动删除 maps/{session_id}/ 图片目录
        let map_dir = std::path::Path::new("maps").join(&sid);
        if map_dir.exists() {
            let _ = std::fs::remove_dir_all(&map_dir);
        }
        // 联动删除 hotels/{session_id}/ 酒店评论图片目录
        let hotel_dir = std::path::Path::new("hotels").join(&sid);
        if hotel_dir.exists() {
            let _ = std::fs::remove_dir_all(&hotel_dir);
        }
        // 联动删除会话记忆笔记
        let notes = notes_path(id);
        if notes.exists() {
            let _ = std::fs::remove_file(&notes);
        }
        Ok(())
    }

    /// 重命名指定 id 的会话（更新 name 字段并保存）。
    pub fn rename(id: &str, name: String) -> Result<()> {
        let mut s = Self::load(id)?;
        s.name = Some(name);
        s.save()
    }

    /// 将尚未归属的旧 Web 会话迁移给指定管理员；已有归属不会覆盖。
    pub fn assign_unowned_to(owner_user_id: i64) -> Result<usize> {
        let metas = Self::list()?;
        let mut count = 0;
        for meta in metas {
            let mut session = Self::load(&meta.id)?;
            if session.owner_user_id.is_none() {
                session.owner_user_id = Some(owner_user_id);
                session.save()?;
                count += 1;
            }
        }
        Ok(count)
    }

    /// 将指定会话的所有者改为目标账号；地图、酒店和记忆文件按 SID 自动随会话转移。
    pub fn transfer_owner(id: &str, owner_user_id: i64) -> Result<()> {
        let mut session = Self::load(id).with_context(|| format!("加载会话 {id} 失败"))?;
        session.owner_user_id = Some(owner_user_id);
        session.save()
    }

    /// 列出所有已存档会话（按修改时间倒序），用于 TUI 选择页。
    pub fn list() -> Result<Vec<SessionMeta>> {
        let dir = std::path::Path::new("sessions");
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut out: Vec<SessionMeta> = Vec::new();
        for entry in std::fs::read_dir(dir).context("读取 sessions 目录失败")? {
            // 并发场景（Web 下列表与增删同时发生）里目录项可能瞬间消失：
            // 单条失败一律跳过，不让整个列表 500（与下方解析失败跳过同策略）
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(json) = std::fs::read_to_string(&path) else {
                continue;
            };
            let s: Session = match parse_session_json(&json) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let id = path
                .file_stem()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_string();
            // 不在列表里显示 CLI 临时对话
            if id == "cli" {
                continue;
            }
            out.push(SessionMeta {
                id,
                messages: s.messages.len(),
                prompt_tokens: s.usage.prompt_tokens,
                completion_tokens: s.usage.completion_tokens,
                total_tokens: s.usage.total_tokens,
                // 用 session 内记录的时间，不受文件 mtime 影响（trim_all 不刷时间）
                updated_at: s.updated_at,
                name: s.name,
                owner_user_id: s.owner_user_id,
                workflow_mode: s.workflow_mode,
                quick_mode: s.quick_mode,
            });
        }
        // 按最后对话时间倒序（最新在前）
        out.sort_by_key(|m| std::cmp::Reverse(m.updated_at));
        Ok(out)
    }

    /// 裁剪到最近 max 条；None 表示不限。
    pub fn trim_messages(&mut self, max: Option<usize>) {
        if let Some(max) = max
            && self.messages.len() > max
        {
            let start = self.messages.len() - max;
            self.messages.drain(..start);
        }
    }

    /// 删除旧会话，仅保留最近 keep 个；None 表示不限（cli 天然永久保留）。
    /// 联动删除被清理会话的地图图片目录。
    pub fn prune(keep: Option<usize>) -> Result<()> {
        let metas = Self::list()?;
        if let Some(keep) = keep {
            for m in metas.into_iter().skip(keep) {
                let _ = Self::delete(&m.id);
            }
        }
        Ok(())
    }

    /// 启动时遍历所有会话，把消息裁剪到最近 max 条；None 表示不限。
    pub fn trim_all(max: Option<usize>) -> Result<()> {
        if max.is_none() {
            return Ok(());
        }
        for m in Self::list()? {
            if let Ok(mut s) = Self::load(&m.id) {
                s.trim_messages(max);
                let _ = s.save();
            }
        }
        Ok(())
    }
}

/// 会话列表项（TUI 选择页用）。
#[derive(Clone)]
pub struct SessionMeta {
    pub id: String,
    pub messages: usize,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    /// 最后对话时间（来自 session.updated_at，不受文件 mtime 影响）
    pub updated_at: Option<std::time::SystemTime>,
    /// 显示名（可中文），None 时显示 id
    pub name: Option<String>,
    /// Web 账号所有者；认证关闭或 TUI/旧会话为 None。
    pub owner_user_id: Option<i64>,
    /// 顶层工作流；旧列表项缺失时按完整攻略兼容。
    pub workflow_mode: WorkflowMode,
    /// 快捷模式子类型。
    pub quick_mode: QuickMode,
}

/// 把 id 里的特殊字符替换成 _，避免路径越界。
fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// 兼容早期版本写出的尾逗号 JSON（`,}` / `,]`）。
/// 新保存一律使用 serde_json 正常格式；这里只在严格解析失败时作为读取兜底，
/// 并且只移除字符串之外、紧邻闭合括号的逗号，避免改动正文内容。
fn parse_session_json(json: &str) -> serde_json::Result<Session> {
    match serde_json::from_str(json) {
        Ok(session) => Ok(session),
        Err(first_error) => {
            let cleaned = strip_trailing_commas(json);
            serde_json::from_str(&cleaned).map_err(|_| first_error)
        }
    }
}

fn strip_trailing_commas(json: &str) -> String {
    let chars: Vec<char> = json.chars().collect();
    let mut out = String::with_capacity(json.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if ch == '"' {
            in_string = true;
            out.push(ch);
            i += 1;
            continue;
        }
        if ch == ',' {
            let mut next = i + 1;
            while next < chars.len() && chars[next].is_whitespace() {
                next += 1;
            }
            if next < chars.len() && matches!(chars[next], '}' | ']') {
                i += 1;
                continue;
            }
        }
        out.push(ch);
        i += 1;
    }
    out
}

/// 会话记忆笔记路径：sessions/{id}.md（LLM 经 update_notes 工具维护）。
/// 与 {id}.json 同目录、同 sanitize 规则，删除会话时联动清理。
fn notes_path(id: &str) -> std::path::PathBuf {
    let mut p = std::path::PathBuf::from("sessions");
    p.push(format!("{}.md", sanitize(id)));
    p
}

/// 读取会话记忆笔记；文件不存在返回 None（旧会话无笔记，正常）。
pub fn load_notes(id: &str) -> Result<Option<String>> {
    let path = notes_path(id);
    if !path.exists() {
        return Ok(None);
    }
    let s =
        std::fs::read_to_string(&path).with_context(|| format!("读取 {} 失败", path.display()))?;
    Ok(Some(s))
}

/// 保存会话记忆笔记（整文件重写）。content 为空视为清空。
pub fn save_notes(id: &str, content: &str) -> Result<()> {
    let path = notes_path(id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("创建 sessions 目录失败")?;
    }
    std::fs::write(&path, content).with_context(|| format!("写入 {} 失败", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_session_greeting_explains_both_modes_and_boundaries() {
        let session = Session::new("test-greeting-modes".into());
        let greeting = session.messages[0]
            .content
            .as_deref()
            .expect("新会话应有固定欢迎语");
        assert!(greeting.contains("拾光者"));
        assert!(greeting.contains("快捷模式") && greeting.contains("完整攻略"));
        for capability in ["种草", "排程", "地图", "小红书", "携程候选酒店核验"] {
            assert!(greeting.contains(capability), "欢迎语应包含 {capability}");
        }
        for stage in ["需求", "路线", "景点", "住宿", "交通", "最终攻略"] {
            assert!(greeting.contains(stage), "完整攻略说明应包含 {stage}");
        }
        assert!(greeting.contains("知识库目前尚未启用"));
        assert!(greeting.ends_with("？"));
    }

    #[test]
    fn new_session_can_persist_selected_workflow_mode() {
        let quick =
            Session::new_with_workflow_mode("test-selected-quick".into(), WorkflowMode::Quick);
        let full = Session::new_with_workflow_mode("test-selected-full".into(), WorkflowMode::Full);
        assert_eq!(quick.workflow_mode, WorkflowMode::Quick);
        assert_eq!(full.workflow_mode, WorkflowMode::Full);
        assert_eq!(quick.quick_mode, QuickMode::Auto);
        assert_eq!(full.quick_mode, QuickMode::Auto);
    }

    /// 记忆笔记读写往返 + 删除联动（本地文件操作，不联网）。
    #[test]
    fn notes_roundtrip_and_delete() {
        let id = "test-notes-roundtrip";
        // 确保起点干净
        let _ = Session::delete(id);

        // 不存在 → None
        assert_eq!(load_notes(id).unwrap(), None);

        // 保存 → 读取一致
        save_notes(id, "# 已确认信息\n- 2人").unwrap();
        assert_eq!(load_notes(id).unwrap().unwrap(), "# 已确认信息\n- 2人");

        // 整文件重写
        save_notes(id, "# v2").unwrap();
        assert_eq!(load_notes(id).unwrap().unwrap(), "# v2");

        // delete 联动删笔记
        save_notes(id, "").unwrap(); // Session::delete 只在 json 存在时删文件，先落一个 json
        Session::new(id.into()).save().unwrap();
        Session::delete(id).unwrap();
        assert_eq!(load_notes(id).unwrap(), None);
    }

    #[test]
    fn owner_defaults_and_old_json_compatibility() {
        let session = Session::new("test-owner-migration".into());
        assert_eq!(session.owner_user_id, None);
        assert_eq!(session.workflow_mode, WorkflowMode::Quick);
        assert_eq!(session.quick_mode, QuickMode::Auto);
        let json = serde_json::to_string(&session).unwrap();
        let restored: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.owner_user_id, None);
        let mut old_value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let object = old_value.as_object_mut().unwrap();
        object.remove("owner_user_id");
        object.remove("workflow_mode");
        object.remove("quick_mode");
        let old_json = serde_json::to_string(&old_value).unwrap();
        let restored_old: Session = serde_json::from_str(&old_json).unwrap();
        assert_eq!(restored_old.owner_user_id, None);
        assert_eq!(restored_old.workflow_mode, WorkflowMode::Full);
        assert_eq!(restored_old.quick_mode, QuickMode::Auto);

        let trailing = json.replace("\n}", ",\n}");
        let restored_trailing = parse_session_json(&trailing).unwrap();
        assert_eq!(restored_trailing.id, "test-owner-migration");
    }

    #[test]
    fn transfer_owner_keeps_session_data() {
        let id = "test-owner-transfer";
        let _ = Session::delete(id);
        let mut session = Session::new(id.into());
        session.owner_user_id = Some(11);
        session.save().unwrap();

        Session::transfer_owner(id, 22).unwrap();
        let transferred = Session::load(id).unwrap();
        assert_eq!(transferred.owner_user_id, Some(22));
        assert_eq!(transferred.messages.len(), 1);
        Session::delete(id).unwrap();
    }
}
