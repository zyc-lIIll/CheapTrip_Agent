use anyhow::{Result, bail};
use futures::StreamExt;
use serde::Serialize;
use std::io::Write;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

use crate::llm::{self, ChatRequest, FunctionCall, Message, StreamEvent, ToolCall, Usage};
use crate::session::{QuickMode, Session, WorkflowMode};
use crate::tools::{self, DynTool};

/// system prompt 按阶段动态组装（渐进披露）：总则常驻，阶段指令随 session.phase 切换。
/// 文件仍在编译期 include_str! 嵌入，运行时只做零成本拼接；改 prompt 只需改 .md 文件。
/// 总则：人设 + part 概念 + 工具说明 + 五阶段总览 + 约束（常驻，让 LLM 全程知道全貌）。
const SYSTEM_CORE: &str = include_str!("../skills/system.md");

fn current_beijing_date() -> String {
    let days = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
        + 8 * 3600)
        .div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}")
}
/// 各阶段详细指令，下标 0-4 ↔ 阶段0-4，下标 5 ↔ 阶段5。
const PHASE_PROMPTS: [&str; 6] = [
    include_str!("../skills/phases/0.md"),
    include_str!("../skills/phases/1.md"),
    include_str!("../skills/phases/2.md"),
    include_str!("../skills/phases/3.md"),
    include_str!("../skills/phases/4.md"),
    include_str!("../skills/phases/5.md"),
];

/// 技能文档：按阶段挂载（见 `skill_docs_for` 的写死映射表）。
const MAP_MD: &str = include_str!("../skills/map.md");
const SPECIAL_MD: &str = include_str!("../skills/special.md");
const HOTEL_MD: &str = include_str!("../skills/hotel.md");
const DISCOVERY_MD: &str = include_str!("../skills/discovery.md");
/// 小红书搜索技能：仅当 config.toml [xhs].enabled = true 时挂载（工具同步注册）。
const XHS_MD: &str = include_str!("../skills/xhs.md");
const QUICK_CORE: &str = include_str!("../skills/quick/system.md");
const QUICK_PROMPTS: [&str; 7] = [
    include_str!("../skills/quick/auto.md"),
    include_str!("../skills/quick/inspiration.md"),
    include_str!("../skills/quick/schedule.md"),
    include_str!("../skills/quick/map.md"),
    include_str!("../skills/quick/xhs.md"),
    include_str!("../skills/quick/ctrip.md"),
    include_str!("../skills/quick/knowledge.md"),
];

fn is_title_punctuation(ch: char) -> bool {
    matches!(
        ch,
        ',' | '，'
            | '.'
            | '。'
            | ':'
            | '：'
            | ';'
            | '；'
            | '!'
            | '！'
            | '?'
            | '？'
            | '、'
            | '-'
            | '—'
            | '_'
            | '…'
            | '"'
            | '\''
            | '“'
            | '”'
            | '‘'
            | '’'
            | '('
            | ')'
            | '（'
            | '）'
            | '['
            | ']'
            | '【'
            | '】'
    )
}

fn title_from_first_message(input: &str) -> String {
    let normalized = input.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut title = normalized.as_str();
    for prefix in ["可以帮我", "请帮我", "能不能", "帮我", "我想", "请"] {
        if let Some(rest) = title.strip_prefix(prefix) {
            title =
                rest.trim_start_matches(|ch: char| ch.is_whitespace() || is_title_punctuation(ch));
            break;
        }
    }
    let title = title.trim_matches(|ch: char| ch.is_whitespace() || is_title_punctuation(ch));
    let title: String = title.chars().take(15).collect();
    if title.is_empty() {
        "新对话".into()
    } else {
        title
    }
}

fn should_name_first_message(session: &Session) -> bool {
    session.name.is_none()
        && !session
            .messages
            .iter()
            .any(|message| message.role == "user")
}

fn name_first_message(session: &mut Session, user: &str) {
    if should_name_first_message(session) {
        session.name = Some(title_from_first_message(user));
    }
}

/// 各阶段挂载的技能文档。代码写死映射，不让 LLM 选；
/// 与各 phases/{n}.md 里引用的技能文档保持一致：
/// 阶段0种草闲聊→无（业务工具仅 search_web/search_xhs，prompt 已写死策略，不挂技能）；
/// 阶段1行李→special；阶段2大局（搜攻略+总览图）→discovery+map；
/// 阶段3逐part（景点+行程+住宿）→discovery+map+special+hotel；
/// 阶段4整体调整（换酒店最重要，防瞎改）→hotel+discovery+special；阶段5完整攻略→map+special。
/// 小红书技能在已启用时随阶段1-5追加（各阶段都有真实体验/避坑/时效查询场景）。
fn skill_docs_for(phase: u8, xhs_enabled: bool) -> Vec<&'static str> {
    let mut docs = match phase {
        0 => vec![],
        1 => vec![SPECIAL_MD],
        2 => vec![DISCOVERY_MD, MAP_MD],
        3 => vec![DISCOVERY_MD, MAP_MD, SPECIAL_MD, HOTEL_MD],
        4 => vec![HOTEL_MD, DISCOVERY_MD, SPECIAL_MD],
        _ => vec![MAP_MD, SPECIAL_MD],
    };
    if xhs_enabled && phase >= 1 {
        docs.push(XHS_MD);
    }
    docs
}
/// agent 向前端推送的事件。实现 Serialize，便于 Web/手机前端 JSON 序列化。
#[derive(Serialize, Clone, Debug)]
pub enum AgentEvent {
    /// 一轮开始
    Step { n: u32 },
    /// 正文增量
    Content(String),
    /// 推理增量
    Reasoning(String),
    /// 工具调用开始
    ToolCall { name: String, args: String },
    /// 工具结果
    ToolResult(String),
    /// 阶段切换
    PhaseChange { phase: u8 },
    /// 一轮结束的最终回复
    Done(String),
    /// token 用量更新
    Usage(Usage),
    /// 错误（含取消）
    Error(String),
}

pub struct Agent {
    client: llm::Client,
    model: String,
    temperature: f32,
    max_tokens: u32,
    tools: Vec<Box<DynTool>>,
    /// 小红书工具是否启用（决定 xhs.md 是否挂载）
    xhs_enabled: bool,
}

/// 流式期间累积一个 tool_call 的片段。
struct ToolCallAcc {
    index: u32,
    id: String,
    name: String,
    args: String,
}

impl Agent {
    pub fn new(client: llm::Client, cfg: &crate::config::Config, tools: Vec<Box<DynTool>>) -> Self {
        Self {
            client,
            model: cfg.llm.model.clone(),
            temperature: cfg.llm.temperature,
            max_tokens: cfg.llm.max_tokens,
            tools,
            xhs_enabled: cfg.xhs.enabled,
        }
    }

    /// 按当前阶段组装 system prompt：总则（含 part 概念）+ 当前阶段指令 + 按阶段挂载的技能文档 + 会话记忆。
    /// phase 越界（>5）时按阶段5兜底；无记忆笔记则省略该段。
    fn prompt_for(session: &Session, xhs_enabled: bool) -> String {
        let mut prompt = if session.workflow_mode == WorkflowMode::Quick {
            let quick_idx = match session.quick_mode {
                QuickMode::Auto => 0,
                QuickMode::Inspiration => 1,
                QuickMode::Schedule => 2,
                QuickMode::Map => 3,
                QuickMode::Xhs => 4,
                QuickMode::Ctrip => 5,
                QuickMode::Knowledge => 6,
            };
            format!("{QUICK_CORE}\n\n---\n\n{}", QUICK_PROMPTS[quick_idx])
        } else {
            let idx = session.phase.clamp(0, 5) as usize;
            let mut full = format!("{SYSTEM_CORE}\n\n---\n\n{}", PHASE_PROMPTS[idx]);
            let docs = skill_docs_for(session.phase, xhs_enabled);
            if !docs.is_empty() {
                full.push_str("\n\n---\n\n");
                full.push_str(&docs.join("\n\n---\n\n"));
            }
            full
        };
        if session.workflow_mode == WorkflowMode::Quick
            && matches!(session.quick_mode, QuickMode::Xhs | QuickMode::Inspiration)
            && !xhs_enabled
        {
            prompt.push_str(
                "\n\n---\n\n当前环境未启用小红书服务；不要调用小红书工具，须按快捷提示明确降级。\n",
            );
        }
        prompt.push_str(&format!(
            "\n\n---\n\n## 当前时间基准\n当前日期（北京时间）：{}。用户只写月日时，必须结合该日期推断最近的未来日期；不得擅自使用过去的年份。",
            current_beijing_date()
        ));
        if let Ok(Some(notes)) = crate::session::load_notes(&session.id)
            && !notes.trim().is_empty()
        {
            prompt
                .push_str("\n\n---\n\n# 会话记忆（当前已确认信息，经 update_notes 工具维护）\n\n");
            prompt.push_str(&notes);
        }
        prompt
    }

    fn system_prompt_for(&self, session: &Session) -> String {
        Self::prompt_for(session, self.xhs_enabled)
    }
    /// 当前 Session 可见的工具名称。白名单在 Agent 内集中计算，避免只靠 prompt 约束模型。
    fn allowed_tool_names(session: &Session) -> &'static [&'static str] {
        const AUTO: &[&str] = &["set_mode", "update_notes", "search_trains"];
        const INSPIRATION: &[&str] = &[
            "set_mode",
            "update_notes",
            "search_trains",
            "search_web",
            "search_xhs",
            "read_xhs_note",
        ];
        const SCHEDULE: &[&str] = &[
            "set_mode",
            "update_notes",
            "search_web",
            "search_trains",
            "get_weather",
            "geocode",
            "route_check",
            "generate_overview_map",
            "generate_city_map",
        ];
        const MAP: &[&str] = &[
            "set_mode",
            "update_notes",
            "search_trains",
            "geocode",
            "route_check",
            "generate_overview_map",
            "generate_city_map",
        ];
        const XHS: &[&str] = &[
            "set_mode",
            "update_notes",
            "search_trains",
            "search_xhs",
            "read_xhs_note",
        ];
        const CTRIP: &[&str] = &[
            "set_mode",
            "update_notes",
            "search_trains",
            "search_web",
            "search_hotel_reviews",
        ];
        const KNOWLEDGE: &[&str] = &["set_mode", "update_notes", "search_trains"];
        const FULL_PHASE0: &[&str] = &[
            "set_phase",
            "update_notes",
            "search_trains",
            "search_web",
            "search_xhs",
            "read_xhs_note",
        ];
        const FULL_PHASE1: &[&str] = &[
            "set_phase",
            "update_notes",
            "search_web",
            "search_xhs",
            "read_xhs_note",
            "search_trains",
            "get_weather",
            "geocode",
            "generate_overview_map",
        ];
        const FULL_PHASE2: &[&str] = &[
            "set_phase",
            "update_notes",
            "search_web",
            "search_xhs",
            "read_xhs_note",
            "search_trains",
            "geocode",
            "cluster_pois",
            "route_check",
            "generate_overview_map",
        ];
        const FULL_PHASE3: &[&str] = &[
            "set_phase",
            "update_notes",
            "search_web",
            "search_xhs",
            "read_xhs_note",
            "search_trains",
            "get_weather",
            "geocode",
            "route_check",
            "generate_city_map",
            "search_hotel_reviews",
        ];
        const FULL_PHASE4: &[&str] = &[
            "set_phase",
            "update_notes",
            "search_web",
            "search_xhs",
            "read_xhs_note",
            "search_trains",
            "get_weather",
            "geocode",
            "route_check",
            "generate_city_map",
            "search_hotel_reviews",
        ];
        const FULL_PHASE5: &[&str] = &[
            "set_phase",
            "update_notes",
            "search_web",
            "search_xhs",
            "read_xhs_note",
            "search_trains",
            "get_weather",
            "geocode",
            "route_check",
            "generate_overview_map",
            "generate_city_map",
            "search_hotel_reviews",
        ];

        match session.workflow_mode {
            WorkflowMode::Full => match session.phase.clamp(0, 5) {
                0 => FULL_PHASE0,
                1 => FULL_PHASE1,
                2 => FULL_PHASE2,
                3 => FULL_PHASE3,
                4 => FULL_PHASE4,
                _ => FULL_PHASE5,
            },
            WorkflowMode::Quick => match session.quick_mode {
                QuickMode::Auto => AUTO,
                QuickMode::Inspiration => INSPIRATION,
                QuickMode::Schedule => SCHEDULE,
                QuickMode::Map => MAP,
                QuickMode::Xhs => XHS,
                QuickMode::Ctrip => CTRIP,
                QuickMode::Knowledge => KNOWLEDGE,
            },
        }
    }

    fn ensure_tool_allowed(session: &Session, name: &str) -> Result<()> {
        if !Self::allowed_tool_names(session).contains(&name) {
            bail!(
                "当前模式不允许调用工具 {name}（workflow_mode={:?}, quick_mode={:?}, phase={}）",
                session.workflow_mode,
                session.quick_mode,
                session.phase
            );
        }
        Ok(())
    }

    fn apply_quick_mode(session: &mut Session, args: &serde_json::Value) -> Result<String> {
        if session.workflow_mode != WorkflowMode::Quick {
            bail!("完整攻略模式不允许调用 set_mode");
        }
        let mode = tools::mode::parse_quick_mode(args)?;
        session.quick_mode = mode;
        Ok(format!(
            "已切换快捷任务类型为 {}",
            serde_json::to_string(&mode)?
        ))
    }

    fn llm_tools_for(session: &Session, registered: &[Box<DynTool>]) -> Vec<llm::Tool> {
        registered
            .iter()
            .filter(|t| Self::allowed_tool_names(session).contains(&t.name()))
            .map(|t| tools::llm_tool(&**t))
            .collect()
    }

    fn llm_tools(&self, session: &Session) -> Vec<llm::Tool> {
        Self::llm_tools_for(session, &self.tools)
    }

    async fn dispatch(&self, session: &Session, call: &llm::ToolCall) -> Result<String> {
        let name = &call.function.name;
        Self::ensure_tool_allowed(session, name)?;
        let args = tools::parse_args(call.function.arguments.as_deref(), name)?;
        for t in &self.tools {
            if t.name() == name {
                let dt: &DynTool = &**t;
                return dt.execute(args).await;
            }
        }
        bail!("未知工具: {name}");
    }

    /// 跑一轮：把 user 消息加进 session，流式驱动 LLM↔工具 循环，结果写回 session。
    /// 进度通过 events 推送；cancel 触发则干净中止。
    pub async fn run(
        &self,
        session: &mut Session,
        user: &str,
        events: Sender<AgentEvent>,
        cancel: CancellationToken,
    ) -> Result<()> {
        name_first_message(session, user);
        session.messages.push(Message {
            role: "user".into(),
            content: Some(user.into()),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        });

        for step in 0..20u32 {
            let _ = events.send(AgentEvent::Step { n: step }).await;
            // 每次请求 prepend 当前阶段的 system prompt（不存进 session，避免历史膨胀）
            let mut messages = Vec::with_capacity(session.messages.len() + 1);
            messages.push(Message {
                role: "system".into(),
                content: Some(self.system_prompt_for(session)),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            });
            messages.extend(session.messages.iter().cloned());
            let mut req = ChatRequest {
                model: self.model.clone(),
                messages,
                tools: self.llm_tools(session),
                temperature: Some(self.temperature),
                max_tokens: Some(self.max_tokens),
                stream: None,
                stream_options: None,
                reasoning_effort: None,
            };

            let mut content = String::new();
            let mut reasoning = String::new();
            let mut accs: Vec<ToolCallAcc> = Vec::new();
            let mut call_usage = Usage::default();
            // 跨续写调用累计 usage（每次续写都是一次完整 LLM 调用，费用要算全）
            let mut usage_sum = Usage::default();
            let mut continuations = 0u32;
            // 工具调用中途被截断的重试计数（丢弃半截参数，请模型重发精简版）
            let mut tool_tries = 0u32;
            // 起始断流重试：建流失败/一字未收即断（网关抖动），同一请求原样重发
            let mut stream_attempts = 0u32;
            let mut need_retry;
            let mut last_err: Option<anyhow::Error> = None;

            'stream: loop {
                need_retry = false;
                let stream_opt = match self.client.chat_stream(req.clone()).await {
                    Ok(s) => Some(s),
                    Err(e) => {
                        last_err = Some(e);
                        need_retry = true;
                        None
                    }
                };
                let mut need_continue = false;
                let mut need_tool_retry = false;
                let mut completed = false;
                if let Some(mut stream) = stream_opt {
                    loop {
                        let next = tokio::select! {
                            biased;
                            _ = cancel.cancelled() => {
                                return Err(anyhow::anyhow!("已取消"));
                            }
                            n = stream.next() => n,
                        };
                        match next {
                            // 流被掐断（没收到 [DONE] 就断了）：按断点类型分流
                            None => {
                                if content.is_empty() && accs.is_empty() {
                                    last_err =
                                        Some(anyhow::anyhow!("LLM 流式连接中断（未收到数据）"));
                                    need_retry = true;
                                } else if accs.is_empty() {
                                    // 有正文无工具调用：从断点续写
                                    need_continue = true;
                                } else if tool_tries < 3 {
                                    // 半截工具调用：丢弃并请模型重发（有界）
                                    need_tool_retry = true;
                                } else {
                                    last_err = Some(anyhow::anyhow!(
                                        "LLM 流在工具调用中途中断且重试用尽；\
                                         请检查网络或调大 config.toml [llm] max_tokens"
                                    ));
                                }
                                break;
                            }
                            Some(Err(e)) => {
                                last_err = Some(e);
                                if content.is_empty() && accs.is_empty() {
                                    // 一个字都没收到：网关抖动，重试同一请求
                                    need_retry = true;
                                } else if accs.is_empty() {
                                    need_continue = true;
                                } else if tool_tries < 3 {
                                    need_tool_retry = true;
                                }
                                break;
                            }
                            Some(Ok(ev)) => match ev {
                                StreamEvent::Content(s) => {
                                    let _ = events.send(AgentEvent::Content(s.clone())).await;
                                    content.push_str(&s);
                                }
                                StreamEvent::Reasoning(s) => {
                                    let _ = events.send(AgentEvent::Reasoning(s.clone())).await;
                                    reasoning.push_str(&s);
                                }
                                StreamEvent::ToolCallDelta {
                                    index,
                                    id,
                                    name,
                                    args_chunk,
                                } => {
                                    if let Some(a) = accs.iter_mut().find(|a| a.index == index) {
                                        if let Some(i) = id {
                                            a.id = i;
                                        }
                                        if let Some(n) = name {
                                            a.name = n;
                                        }
                                        if let Some(c) = args_chunk {
                                            a.args.push_str(&c);
                                        }
                                    } else {
                                        accs.push(ToolCallAcc {
                                            index,
                                            id: id.unwrap_or_default(),
                                            name: name.unwrap_or_default(),
                                            args: args_chunk.unwrap_or_default(),
                                        });
                                    }
                                }
                                StreamEvent::Usage(u) => call_usage = u,
                                StreamEvent::Finish { reason } => {
                                    if reason.as_deref() == Some("length") {
                                        if accs.is_empty() && !content.is_empty() {
                                            // 纯正文被 max_tokens 截断：从断点续写（Aider infinite-output 式）
                                            need_continue = true;
                                            break;
                                        }
                                        if !accs.is_empty() {
                                            // 截断落在工具调用参数里：丢弃半截参数，请模型重发（有界）
                                            if tool_tries < 3 {
                                                need_tool_retry = true;
                                            } else {
                                                last_err = Some(anyhow::anyhow!(
                                                    "输出在工具调用中途被 max_tokens 截断（已重试 3 次）；\
                                                     请调大 config.toml [llm] max_tokens 或让模型精简工具参数"
                                                ));
                                            }
                                            break;
                                        }
                                    }
                                }
                                StreamEvent::Done => {
                                    completed = true;
                                    break;
                                }
                            },
                        }
                    }
                }
                // 本次调用的 usage 计入总和（Done/截断/断流所有路径都会经过这里），然后重置
                usage_sum.prompt_tokens += call_usage.prompt_tokens;
                usage_sum.completion_tokens += call_usage.completion_tokens;
                usage_sum.total_tokens += call_usage.total_tokens;
                call_usage = Usage::default();

                if completed {
                    break 'stream;
                }
                if need_retry && stream_attempts < 3 {
                    stream_attempts += 1;
                    // 退避后原样重发（可被取消打断）
                    let backoff = std::time::Duration::from_millis(1000 * stream_attempts as u64);
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return Err(anyhow::anyhow!("已取消")),
                        _ = tokio::time::sleep(backoff) => {}
                    }
                    continue 'stream;
                }
                if need_continue && continuations < 3 {
                    continuations += 1;
                    // 已收到的部分内容作为 assistant 消息，追加「继续」让模型从断点接着写
                    req.messages.push(Message {
                        role: "assistant".into(),
                        content: Some(content.clone()),
                        tool_calls: None,
                        tool_call_id: None,
                        reasoning_content: None,
                    });
                    req.messages.push(Message {
                        role: "user".into(),
                        content: Some("继续，从中断处接着输出，不要重复已输出的内容。".into()),
                        tool_calls: None,
                        tool_call_id: None,
                        reasoning_content: None,
                    });
                    continue 'stream;
                }
                if need_tool_retry {
                    tool_tries += 1;
                    // 截断发生在工具调用参数中：丢弃半截调用，已有正文存档，
                    // 请模型重新完整调用该工具（精简参数），避免落进 JSON 解析报错
                    accs.clear();
                    if !content.is_empty() {
                        req.messages.push(Message {
                            role: "assistant".into(),
                            content: Some(std::mem::take(&mut content)),
                            tool_calls: None,
                            tool_call_id: None,
                            reasoning_content: None,
                        });
                    }
                    reasoning.clear();
                    let _ = events
                        .send(AgentEvent::Content(format!(
                            "\n\n（工具调用参数被输出长度限制截断，已要求模型重发精简版，第 {tool_tries}/3 次）\n\n"
                        )))
                        .await;
                    req.messages.push(Message {
                        role: "user".into(),
                        content: Some(
                            "你上一条回复中的工具调用参数因输出长度限制被截断，已丢弃。\
                             请重新完整调用该工具：参数必须完整合法，内容尽量精简\
                             （如会话记忆压缩到关键信息即可），不要重复输出之前已确认的正文。"
                                .into(),
                        ),
                        tool_calls: None,
                        tool_call_id: None,
                        reasoning_content: None,
                    });
                    continue 'stream;
                }
                // 走到这里说明没能续写/重试：有错误就报错（空内容或半截工具调用无法续）
                if let Some(e) = last_err.take() {
                    return Err(e);
                }
                break 'stream;
            }

            session.usage.prompt_tokens += usage_sum.prompt_tokens;
            session.usage.completion_tokens += usage_sum.completion_tokens;
            session.usage.total_tokens += usage_sum.total_tokens;
            let _ = events.send(AgentEvent::Usage(session.usage.clone())).await;

            let tool_calls: Vec<ToolCall> = accs
                .into_iter()
                .map(|a| ToolCall {
                    id: a.id,
                    kind: "function".into(),
                    function: FunctionCall {
                        name: a.name,
                        arguments: if a.args.is_empty() {
                            None
                        } else {
                            Some(a.args)
                        },
                    },
                })
                .collect();

            let msg = Message {
                role: "assistant".into(),
                content: if content.is_empty() {
                    None
                } else {
                    Some(content.clone())
                },
                tool_calls: if tool_calls.is_empty() {
                    None
                } else {
                    Some(tool_calls.clone())
                },
                tool_call_id: None,
                reasoning_content: if reasoning.is_empty() {
                    None
                } else {
                    Some(reasoning)
                },
            };
            session.messages.push(msg.clone());

            if !tool_calls.is_empty() {
                for call in tool_calls {
                    let _ = events
                        .send(AgentEvent::ToolCall {
                            name: call.function.name.clone(),
                            args: call.function.arguments.clone().unwrap_or_default(),
                        })
                        .await;
                    // set_phase / set_mode 特殊处理：直接更新 session，不走 dispatch。
                    // 先做服务端白名单校验，防止模型伪造未暴露的调用。
                    let out = if call.function.name == "set_phase" {
                        Self::ensure_tool_allowed(session, &call.function.name)?;
                        let args = tools::parse_args(
                            call.function.arguments.as_deref(),
                            &call.function.name,
                        )?;
                        let phase = args.get("phase").and_then(|v| v.as_u64()).unwrap_or(0) as u8;
                        if (0..=5).contains(&phase) {
                            session.phase = phase;
                            let _ = events.send(AgentEvent::PhaseChange { phase }).await;
                        }
                        format!("已记录进入阶段 {phase}，该阶段详细工作指令已生效")
                    } else if call.function.name == "set_mode" {
                        Self::ensure_tool_allowed(session, &call.function.name)?;
                        let args = tools::parse_args(
                            call.function.arguments.as_deref(),
                            &call.function.name,
                        )?;
                        Self::apply_quick_mode(session, &args)?
                    } else {
                        tokio::select! {
                            biased;
                            _ = cancel.cancelled() => {
                                return Err(anyhow::anyhow!("已取消"));
                            }
                            r = self.dispatch(session, &call) => r?,
                        }
                    };
                    let _ = events.send(AgentEvent::ToolResult(out.clone())).await;
                    session.messages.push(Message {
                        role: "tool".into(),
                        tool_call_id: Some(call.id),
                        content: Some(out),
                        tool_calls: None,
                        reasoning_content: None,
                    });
                }
                continue;
            }

            let _ = events.send(AgentEvent::Done(content.clone())).await;
            return Ok(());
        }
        bail!("agent 循环超过 20 步，疑似死循环");
    }
}

/// 丢弃的 sink：不接 UI 时用于调试运行（打印到 stdout/stderr）。
impl AgentEvent {
    pub fn print(&self) {
        match self {
            AgentEvent::Step { n } => println!("\n— LLM 调用 #{n}（流式）—"),
            AgentEvent::Content(s) => {
                print!("{s}");
                let _ = std::io::stdout().flush();
            }
            AgentEvent::Reasoning(s) => {
                eprint!("\x1b[2m{s}\x1b[0m");
                let _ = std::io::stderr().flush();
            }
            AgentEvent::ToolCall { name, args } => println!("  → 执行工具 {name} ({args})"),
            AgentEvent::ToolResult(r) => println!("  ← {r}"),
            AgentEvent::PhaseChange { phase } => println!("  [进入阶段 {phase}]"),
            AgentEvent::Done(r) => println!("\n[完成] {r}"),
            AgentEvent::Usage(u) => println!(
                "  [tokens in·out·total={}·{}·{}]",
                u.prompt_tokens, u.completion_tokens, u.total_tokens
            ),
            AgentEvent::Error(e) => println!("\n[错误] {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quick_session(mode: QuickMode) -> Session {
        let mut session = Session::new("test-agent-mode".into());
        session.quick_mode = mode;
        session
    }

    #[test]
    fn quick_tool_allowlist_matches_mode_boundaries() {
        let cases = [
            (QuickMode::Auto, &["set_mode", "update_notes"][..]),
            (
                QuickMode::Inspiration,
                &["search_web", "search_xhs", "read_xhs_note"][..],
            ),
            (
                QuickMode::Schedule,
                &[
                    "search_trains",
                    "get_weather",
                    "route_check",
                    "generate_city_map",
                ][..],
            ),
            (
                QuickMode::Map,
                &["geocode", "route_check", "generate_overview_map"][..],
            ),
            (QuickMode::Xhs, &["search_xhs", "read_xhs_note"][..]),
            (
                QuickMode::Ctrip,
                &["search_web", "search_hotel_reviews"][..],
            ),
            (QuickMode::Knowledge, &["set_mode"][..]),
        ];
        for (mode, expected) in cases {
            let allowed = Agent::allowed_tool_names(&quick_session(mode));
            assert!(
                allowed.contains(&"search_trains"),
                "{mode:?} 应允许查 12306"
            );
            for name in expected {
                assert!(allowed.contains(name), "{mode:?} 应允许 {name}");
            }
            assert!(!allowed.contains(&"set_phase"), "{mode:?} 不得进入阶段流程");
        }
    }

    #[test]
    fn full_mode_allows_phase_tools_but_not_set_mode() {
        let mut session = Session::new("test-agent-full".into());
        session.workflow_mode = WorkflowMode::Full;
        session.phase = 3;
        let allowed = Agent::allowed_tool_names(&session);
        assert!(allowed.contains(&"set_phase"));
        assert!(allowed.contains(&"generate_city_map"));
        assert!(!allowed.contains(&"set_mode"));
    }

    #[test]
    fn full_phase1_allows_candidate_location_map() {
        let mut session = Session::new("test-agent-full-phase1".into());
        session.workflow_mode = WorkflowMode::Full;
        session.phase = 1;
        let allowed = Agent::allowed_tool_names(&session);
        assert!(allowed.contains(&"generate_overview_map"));
    }

    #[test]
    fn full_all_phases_allow_trains() {
        for phase in 0..=5 {
            let mut session = Session::new(format!("test-full-phase-{phase}"));
            session.workflow_mode = WorkflowMode::Full;
            session.phase = phase;
            let allowed = Agent::allowed_tool_names(&session);
            assert!(
                allowed.contains(&"search_trains"),
                "阶段 {phase} 应允许查 12306"
            );
        }
    }

    #[test]
    fn physical_registration_limits_visible_tools() {
        let registered: Vec<Box<DynTool>> = vec![
            Box::new(tools::SetMode),
            Box::new(tools::SearchTrains::new()),
        ];
        let visible = Agent::llm_tools_for(&quick_session(QuickMode::Inspiration), &registered);
        let names: Vec<_> = visible
            .iter()
            .map(|tool| tool.function.name.as_str())
            .collect();
        assert_eq!(names, vec!["set_mode", "search_trains"]);
        assert!(!names.contains(&"search_xhs"));
        assert!(!names.contains(&"search_hotel_reviews"));
    }

    #[test]
    fn dispatch_permission_check_rejects_cross_mode_calls() {
        let quick = quick_session(QuickMode::Map);
        assert!(Agent::ensure_tool_allowed(&quick, "set_phase").is_err());
        assert!(Agent::ensure_tool_allowed(&quick, "set_mode").is_ok());

        let mut full = Session::new("test-agent-full-dispatch".into());
        full.workflow_mode = WorkflowMode::Full;
        assert!(Agent::ensure_tool_allowed(&full, "set_phase").is_ok());
        assert!(Agent::ensure_tool_allowed(&full, "set_mode").is_err());
        assert!(Agent::ensure_tool_allowed(&full, "not_registered").is_err());
    }

    #[test]
    fn auto_mode_switch_changes_next_request_allowlist() {
        let registered: Vec<Box<DynTool>> = vec![
            Box::new(tools::SetMode),
            Box::new(tools::SearchWeb::new("test-key".into())),
        ];
        let mut session = quick_session(QuickMode::Auto);
        let auto_tools = Agent::llm_tools_for(&session, &registered);
        assert_eq!(
            auto_tools
                .iter()
                .map(|tool| tool.function.name.as_str())
                .collect::<Vec<_>>(),
            vec!["set_mode"]
        );

        let out =
            Agent::apply_quick_mode(&mut session, &serde_json::json!({"mode": "inspiration"}))
                .expect("快捷模式切换应成功");
        assert_eq!(session.quick_mode, QuickMode::Inspiration);
        assert!(out.contains("inspiration"));
        let next_tools = Agent::llm_tools_for(&session, &registered);
        assert_eq!(
            next_tools
                .iter()
                .map(|tool| tool.function.name.as_str())
                .collect::<Vec<_>>(),
            vec!["set_mode", "search_web"]
        );
    }

    #[test]
    fn full_mode_cannot_apply_set_mode() {
        let mut session = Session::new("test-agent-full-mode".into());
        session.workflow_mode = WorkflowMode::Full;
        assert!(
            Agent::apply_quick_mode(&mut session, &serde_json::json!({"mode": "map"})).is_err()
        );
        assert_eq!(session.workflow_mode, WorkflowMode::Full);
    }

    #[test]
    fn quick_prompt_isolated_from_full_phase_prompt() {
        let mut quick = quick_session(QuickMode::Schedule);
        quick.phase = 4;
        let mut full = Session::new("test-agent-full".into());
        full.phase = 4;
        full.workflow_mode = WorkflowMode::Full;
        let quick_prompt = Agent::prompt_for(&quick, false);
        let full_prompt = Agent::prompt_for(&full, false);
        assert!(quick_prompt.contains("快速排程"));
        assert!(!quick_prompt.contains("阶段 4"));
        assert!(full_prompt.contains("阶段4"));
        assert!(!full_prompt.contains("快捷任务入口"));
    }

    #[test]
    fn train_request_is_visible_to_llm_in_quick_and_full_modes() {
        let registered: Vec<Box<DynTool>> = vec![Box::new(tools::SearchTrains::new())];
        let mut quick = quick_session(QuickMode::Schedule);
        quick.messages.push(Message {
            role: "user".into(),
            content: Some("帮我查郑州到西安的高铁".into()),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        });
        let quick_names = Agent::llm_tools_for(&quick, &registered);
        assert_eq!(quick_names.len(), 1);
        assert_eq!(quick_names[0].function.name, "search_trains");

        let mut full = Session::new("test-train-full".into());
        full.workflow_mode = WorkflowMode::Full;
        let full_names = Agent::llm_tools_for(&full, &registered);
        assert_eq!(full_names.len(), 1);
        assert_eq!(full_names[0].function.name, "search_trains");
    }

    #[test]
    fn prompt_does_not_duplicate_new_session_greeting() {
        let quick = quick_session(QuickMode::Auto);
        let quick_prompt = Agent::prompt_for(&quick, false);
        assert!(!quick_prompt.contains("首轮模式说明"));

        let mut continued = quick;
        continued.messages.push(Message {
            role: "assistant".into(),
            content: Some("已开始处理".into()),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        });
        assert!(!Agent::prompt_for(&continued, false).contains("首轮模式说明"));

        let mut full = Session::new("test-first-turn-full".into());
        full.workflow_mode = WorkflowMode::Full;
        let full_prompt = Agent::prompt_for(&full, false);
        assert!(!full_prompt.contains("首轮模式说明"));
    }

    #[test]
    fn prompt_exposes_auto_train_routing() {
        let auto = quick_session(QuickMode::Auto);
        let auto_prompt = Agent::prompt_for(&auto, false);
        for keyword in ["火车", "高铁", "动车", "车次", "余票", "时刻"] {
            assert!(auto_prompt.contains(keyword), "自动路由提示缺少 {keyword}");
        }
        assert!(auto_prompt.contains("schedule"));
        assert!(auto_prompt.contains("不得在 auto 里直接查车或误分到 `ctrip`"));
        assert!(auto_prompt.contains("另建“完整攻略”会话"));

        let mut full = Session::new("test-naming-full".into());
        full.workflow_mode = WorkflowMode::Full;
        let full_prompt = Agent::prompt_for(&full, false);
        assert!(full_prompt.contains("窄例外"));
        assert!(full_prompt.contains("search_trains"));
    }

    #[test]
    fn first_message_title_is_deterministic_and_preserves_existing_names() {
        assert_eq!(
            title_from_first_message("  请帮我\n规划   西安  "),
            "规划 西安"
        );
        assert_eq!(
            title_from_first_message("可以帮我查郑州到西安的高铁吗？"),
            "查郑州到西安的高铁吗"
        );
        assert_eq!(title_from_first_message("我想"), "新对话");
        assert_eq!(title_from_first_message("？！"), "新对话");

        let long = title_from_first_message("请帮我北京上海广州深圳成都重庆西安杭州南京");
        assert_eq!(long.chars().count(), 15);
        assert_eq!(long, "北京上海广州深圳成都重庆西安杭");

        for mode in [QuickMode::Auto, QuickMode::Schedule] {
            let mut session = quick_session(mode);
            name_first_message(&mut session, "帮我查明天的高铁");
            assert_eq!(session.name.as_deref(), Some("查明天的高铁"));
        }

        let mut full = Session::new("test-agent-full-first-message".into());
        full.workflow_mode = WorkflowMode::Full;
        name_first_message(&mut full, "帮我规划西安旅行");
        assert_eq!(full.name.as_deref(), Some("规划西安旅行"));

        let mut manual = quick_session(QuickMode::Auto);
        manual.name = Some("人工名称".into());
        name_first_message(&mut manual, "帮我查高铁");
        assert_eq!(manual.name.as_deref(), Some("人工名称"));

        let mut old = quick_session(QuickMode::Auto);
        old.messages.push(Message {
            role: "user".into(),
            content: Some("旧消息".into()),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        });
        name_first_message(&mut old, "新消息");
        assert!(old.name.is_none());
    }

    #[test]
    fn prompt_includes_current_beijing_date() {
        let session = Session::new("prompt-current-date-test".into());
        let prompt = Agent::prompt_for(&session, false);
        assert!(prompt.contains("当前日期（北京时间）："));
        assert!(prompt.contains(&current_beijing_date()));
        assert!(prompt.contains("不得擅自使用过去的年份"));
    }

    #[test]
    fn quick_prompts_cover_all_modes_without_full_workflow_docs() {
        let cases = [
            (QuickMode::Auto, "自动识别"),
            (QuickMode::Inspiration, "轻量种草"),
            (QuickMode::Schedule, "快速排程"),
            (QuickMode::Map, "快速地图"),
            (QuickMode::Xhs, "小红书快捷总结"),
            (QuickMode::Ctrip, "携程候选核验"),
            (QuickMode::Knowledge, "个人知识库预留"),
        ];
        for (mode, heading) in cases {
            let session = quick_session(mode);
            let prompt = Agent::prompt_for(&session, true);
            assert!(prompt.contains(heading), "{mode:?} 应挂载对应快捷 prompt");
            assert!(
                !prompt.contains("# 阶段0"),
                "{mode:?} 不得挂载完整攻略阶段 prompt"
            );
            assert!(
                !prompt.contains("# 阶段1"),
                "{mode:?} 不得挂载完整攻略阶段 prompt"
            );
            assert!(
                !prompt.contains("流程总览"),
                "{mode:?} 不得挂载完整攻略总则"
            );
        }
    }

    #[test]
    fn xhs_prompt_reflects_service_availability() {
        let session = quick_session(QuickMode::Xhs);
        let unavailable = Agent::prompt_for(&session, false);
        assert!(unavailable.contains("当前环境未启用小红书服务"));

        let available = Agent::prompt_for(&session, true);
        assert!(!available.contains("当前环境未启用小红书服务"));
        assert!(available.contains("拟人限流"));
    }

    #[test]
    fn auto_fixed_tool_calls_route_to_each_quick_mode() {
        let cases = [
            ("inspiration", QuickMode::Inspiration, "轻量种草"),
            ("schedule", QuickMode::Schedule, "快速排程"),
            ("map", QuickMode::Map, "快速地图"),
            ("xhs", QuickMode::Xhs, "小红书快捷总结"),
            ("ctrip", QuickMode::Ctrip, "携程候选核验"),
            ("knowledge", QuickMode::Knowledge, "个人知识库预留"),
        ];
        for (raw_mode, expected_mode, heading) in cases {
            let mut session = quick_session(QuickMode::Auto);
            let args = serde_json::json!({"mode": raw_mode});
            Agent::apply_quick_mode(&mut session, &args).expect("auto 的固定 set_mode 调用应成功");
            assert_eq!(session.quick_mode, expected_mode);
            let prompt = Agent::prompt_for(&session, false);
            assert!(prompt.contains(heading), "{raw_mode} 应立即切换 prompt");
        }
    }

    /// 阶段→技能文档映射：各阶段挂载内容符合设计（见 skill_docs_for 注释）。
    #[test]
    fn skill_docs_mapping() {
        // 未启用小红书
        assert_eq!(skill_docs_for(1, false), vec![SPECIAL_MD]);
        assert_eq!(skill_docs_for(2, false), vec![DISCOVERY_MD, MAP_MD]);
        assert_eq!(
            skill_docs_for(3, false),
            vec![DISCOVERY_MD, MAP_MD, SPECIAL_MD, HOTEL_MD]
        );
        assert_eq!(
            skill_docs_for(4, false),
            vec![HOTEL_MD, DISCOVERY_MD, SPECIAL_MD]
        );
        assert_eq!(skill_docs_for(5, false), vec![MAP_MD, SPECIAL_MD]);
        // 阶段0种草闲聊：不挂任何技能文档（业务工具仅搜索，策略写在 0.md）
        assert!(skill_docs_for(0, false).is_empty());
        assert!(skill_docs_for(0, true).is_empty(), "阶段0不挂 xhs.md");
        // 越界兜底：超大值夹到阶段5
        assert_eq!(skill_docs_for(9, false), vec![MAP_MD, SPECIAL_MD]);
        // 启用小红书：阶段1-5追加 xhs.md（各阶段都有真实体验/避坑/时效查询场景）
        for phase in 1..=5u8 {
            assert!(skill_docs_for(phase, true).contains(&XHS_MD));
        }
    }

    /// 端到端实跑验收（联网，花真实 token）：
    /// ① LLM 收到含确认信息的用户消息后，是否按总则主动调 update_notes 维护记忆；
    /// ② 笔记内容是否忠实于用户输入（不编造、数字正确）；
    /// ③ 下一轮 system prompt 是否注入了笔记全文。
    /// 运行：`cargo test agent_notes_live -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn agent_notes_live() {
        dotenvy::dotenv().ok();
        let cfg = crate::config::Config::load().expect("加载配置失败");
        let client = llm::Client::new(&cfg).expect("构建 LLM 客户端失败");
        let exa_key = std::env::var("EXA_API_KEY").expect("EXA_API_KEY 未设置");
        let amap_key = std::env::var("AMAP_API_KEY").expect("AMAP_API_KEY 未设置");
        let limiter = tools::RateLimiter::new(400);
        let tools: Vec<Box<DynTool>> = vec![
            Box::new(tools::GetWeather::new(amap_key.clone(), limiter.clone())),
            Box::new(tools::Geocode::new(amap_key.clone(), limiter.clone())),
            Box::new(tools::SearchWeb::new(exa_key)),
            Box::new(tools::GenerateMap::new(String::new(), "probe-notes".into())),
            Box::new(tools::GenerateCityMap::new(
                amap_key,
                String::new(),
                "probe-notes".into(),
                limiter,
            )),
            Box::new(tools::SetPhase),
            Box::new(tools::UpdateNotes::new("probe-notes".into())),
        ];
        let agent = Agent::new(client, &cfg, tools);
        let mut session = Session::new("probe-notes".into());

        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
        let cancel = CancellationToken::new();
        let printer = tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                ev.print();
            }
        });
        agent
            .run(
                &mut session,
                "我们两口子10月1号到5号从北京去西安玩，高铁往返，预算人均3000，\
                 都不会骑车，行李好拿，房间要两人间。",
                tx,
                cancel,
            )
            .await
            .expect("agent.run 失败");
        let _ = printer.await;

        // ② 笔记应存在且忠实：人数2、日期、高铁、预算等关键事实在笔记里
        let notes = crate::session::load_notes("probe-notes")
            .expect("读笔记失败")
            .expect("LLM 未调用 update_notes，验收不通过");
        println!("\n=== 会话记忆内容 ===\n{notes}\n=== 结束 ===");
        assert!(notes.contains("2"), "笔记应含人数");
        assert!(notes.contains("西安"), "笔记应含目的地");
        assert!(notes.contains("3000"), "笔记应含预算");
        assert!(notes.lines().count() <= 60, "笔记不应超过硬限 60 行");

        // ③ 下一轮 system prompt 应注入笔记全文
        let prompt2 = agent.system_prompt_for(&session);
        assert!(
            prompt2.contains("会话记忆"),
            "第二轮 system prompt 应含记忆段"
        );
        assert!(
            prompt2.contains("西安"),
            "第二轮 system prompt 应注入笔记内容"
        );

        // 清理探针产物
        let _ = std::fs::remove_file("sessions/probe-notes.md");
        let _ = std::fs::remove_file("sessions/probe-notes.json");
        let _ = std::fs::remove_dir_all("maps/probe-notes");
    }
}
