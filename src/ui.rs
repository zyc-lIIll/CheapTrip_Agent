use anyhow::Result;
use async_trait::async_trait;
use crossterm::{
    cursor::{Hide, Show},
    event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures::StreamExt;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use std::io::{self, Stdout};
use tokio::select;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_util::sync::CancellationToken;

use crate::agent::AgentEvent;
use crate::core::{Core, Frontend};
use crate::llm::Usage;
use crate::session::Session;

/// UI 上一条消息的角色。
#[derive(Clone)]
enum Role {
    User,
    Assistant,
    Tool,
    System,
}

/// UI 日志里的一行（带角色着色）。
#[derive(Clone)]
struct LogLine {
    role: Role,
    text: String,
}

pub struct App {
    log: Vec<LogLine>,
    /// 当前正在流式输出的正文（未落行为日志行）。
    cur_content: String,
    cur_reasoning: String,
    input: String,
    /// 输入框光标位置（char 索引，0..=input.len()）
    input_cursor: usize,
    running: bool,
    usage: Usage,
    /// 当前轮的取消令牌（运行时存在）。
    current_cancel: Option<CancellationToken>,
    tx_agent: Sender<(String, CancellationToken)>,
    rx_agent: Receiver<AgentEvent>,
    model: String,
    /// 当前阶段（1-5）
    phase: u8,
    /// 每百万 token 人民币价（input, output）
    cost_per_1m: (f64, f64),
    should_quit: bool,
    /// 是否要切回会话选择页（Ctrl-L 触发）
    switch_session: bool,
    /// 手动滚动偏移（None = 跟随底部；Some(n) = 从顶部第 n 行起）
    scroll: Option<usize>,
    /// render 时算好的「底部偏移」，供按键从底部开始上滚
    bottom_scroll: usize,
    /// 未收到 ToolResult 的工具名队列（agent 按 ToolCall→ToolResult 逐对发送）
    pending_tools: Vec<String>,
}

/// TUI 前端入口结构体；实现 [`Frontend`] trait。
pub struct Tui {
    pub model: String,
    pub history: Vec<crate::llm::Message>,
    pub usage: Usage,
    pub phase: u8,
    /// 每百万 token 人民币价（input, output）
    pub cost_per_1m: (f64, f64),
}

#[async_trait]
impl Frontend for Tui {
    async fn run(self, core: Core) -> Result<bool> {
        run(
            core,
            self.model,
            self.history,
            self.usage,
            self.phase,
            self.cost_per_1m,
        )
        .await
    }
}

/// 启动时的会话选择页：列出历史 + 新建 + 删除，返回选中的 Session。
pub async fn pick_session() -> Result<Session> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, Hide)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut metas = Session::list().unwrap_or_default();
    let mut key_events = EventStream::new();
    let result: Result<Session>;

    let mut selected: usize = 0;
    let mut confirm_delete: Option<String> = None;
    // 重命名模式：(id, 输入缓冲)
    let mut renaming: Option<(String, String)> = None;

    loop {
        let total = metas.len() + 1;

        terminal.draw(|f| {
            let area = f.area();
            if let Some((ref id, ref buf)) = renaming {
                let display = metas
                    .iter()
                    .find(|m| &m.id == id)
                    .map(|m| m.name.clone().unwrap_or_else(|| m.id.clone()))
                    .unwrap_or_else(|| id.clone());
                let text = format!(
                    " 重命名「{display}」\n 输入新名称（可中文）: {buf}▎\n Enter 确认 / Esc 取消"
                );
                f.render_widget(
                    Paragraph::new(text)
                        .block(Block::default().borders(Borders::ALL).title("重命名会话")),
                    area,
                );
                return;
            }
            // 重命名模式渲染见上，下面是删除确认渲染
            if let Some(ref id) = confirm_delete {
                let display = metas
                    .iter()
                    .find(|m| &m.id == id)
                    .map(|m| m.name.clone().unwrap_or_else(|| m.id.clone()))
                    .unwrap_or_else(|| id.clone());
                let text = format!(" 确认删除「{display}」？y 删除 / 其他键取消");
                f.render_widget(
                    Paragraph::new(text)
                        .block(Block::default().borders(Borders::ALL).title("删除确认")),
                    area,
                );
                return;
            }
            let mut items: Vec<ListItem> = Vec::new();
            for m in &metas {
                let display = m.name.clone().unwrap_or_else(|| m.id.clone());
                items.push(ListItem::new(vec![
                    Line::from(vec![
                        Span::styled(
                            format!(" {} ", display),
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(format!(
                            "  {} 条消息 · tokens in·out·total={}·{}·{} · {}",
                            m.messages,
                            m.prompt_tokens,
                            m.completion_tokens,
                            m.total_tokens,
                            ago(&m.updated_at),
                        )),
                    ]),
                    Line::raw(""),
                ]));
            }
            items.push(ListItem::new(Line::from(vec![
                Span::styled(
                    " ➕ 新建会话 ".to_string(),
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "（自动命名）".to_string(),
                    Style::default().fg(Color::DarkGray),
                ),
            ])));
            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title(
                    "选择会话（↑↓ 移动 / Enter 选中 / n 新建 / d 删除 / r 重命名 / q 退出）",
                ))
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
            let mut state = ListState::default();
            state.select(Some(selected));
            f.render_stateful_widget(list, area, &mut state);
        })?;

        let Some(Ok(ev)) = key_events.next().await else {
            continue;
        };
        let Event::Key(k) = ev else {
            continue;
        };

        // 重命名模式：输入文字
        if let Some((ref id, ref mut buf)) = renaming {
            match k.code {
                KeyCode::Enter => {
                    let name = buf.trim().to_string();
                    if !name.is_empty() {
                        let _ = Session::rename(id, name);
                    }
                    metas = Session::list().unwrap_or_default();
                    renaming = None;
                }
                KeyCode::Esc => {
                    renaming = None;
                }
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) => {
                    buf.push(c);
                }
                _ => {}
            }
            continue;
        }

        if let Some(ref id) = confirm_delete {
            if let KeyCode::Char('y') | KeyCode::Char('Y') = k.code {
                let _ = Session::delete(id);
                metas = Session::list().unwrap_or_default();
            }
            confirm_delete = None;
            continue;
        }

        match k.code {
            KeyCode::Down => selected = (selected + 1) % total,
            KeyCode::Up => selected = (selected + total - 1) % total,
            KeyCode::Enter => {
                result = if selected < metas.len() {
                    Session::load(&metas[selected].id)
                } else {
                    Ok(Session::new(new_session_id()))
                };
                break;
            }
            KeyCode::Char('n') => {
                result = Ok(Session::new(new_session_id()));
                break;
            }
            KeyCode::Char('d') => {
                if selected < metas.len() {
                    confirm_delete = Some(metas[selected].id.clone());
                }
            }
            KeyCode::Char('r') => {
                if selected < metas.len() {
                    let old_name = metas[selected]
                        .name
                        .clone()
                        .unwrap_or_else(|| metas[selected].id.clone());
                    renaming = Some((metas[selected].id.clone(), old_name));
                }
            }
            KeyCode::Char('q') | KeyCode::Esc => {
                result = Err(anyhow::anyhow!("用户取消选择会话"));
                break;
            }
            _ => {}
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, Show)?;
    result
}

/// 生成时间戳会话 id，如 20260829-143022。
fn new_session_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 简单把 epoch 秒转成 UTC 日期时间（不需 chrono）
    let days = secs / 86400;
    let secs_of_day = secs % 86400;
    let (y, m, d) = civil_from_days(days as i64);
    let hh = secs_of_day / 3600;
    let mm = (secs_of_day % 3600) / 60;
    let ss = secs_of_day % 60;
    format!("{y:04}-{m:02}-{d:02}-{hh:02}{mm:02}{ss:02}")
}

/// 公历日期（仅用标准库）：days = 自 1970-01-01 起的天数。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (y + if m <= 2 { 1 } else { 0 }, m, d)
}

/// 把修改时间格式化成「X分钟前 / X小时前 / X天前」。
fn ago(mtime: &Option<std::time::SystemTime>) -> String {
    use std::time::SystemTime;
    let Some(t) = mtime else {
        return "未知时间".to_string();
    };
    let Ok(diff) = SystemTime::now().duration_since(*t) else {
        return "未知时间".to_string();
    };
    let s = diff.as_secs();
    if s < 60 {
        "刚刚".to_string()
    } else if s < 3600 {
        format!("{} 分钟前", s / 60)
    } else if s < 86400 {
        format!("{} 小时前", s / 3600)
    } else {
        format!("{} 天前", s / 86400)
    }
}

async fn run(
    core: Core,
    model: String,
    history: Vec<crate::llm::Message>,
    usage: Usage,
    phase: u8,
    cost_per_1m: (f64, f64),
) -> Result<bool> {
    let (tx_agent, rx_agent) = core.into_parts();
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, Hide)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // 显示 session 历史（开场白已在 session 的首条 assistant 消息里，不再硬编码）
    let mut log: Vec<LogLine> = Vec::new();
    for m in &history {
        match m.role.as_str() {
            "user" => log.push(LogLine {
                role: Role::User,
                text: m.content.clone().unwrap_or_default(),
            }),
            "assistant" => log.push(LogLine {
                role: Role::Assistant,
                text: m.content.clone().unwrap_or_default(),
            }),
            "tool" => log.push(LogLine {
                role: Role::Tool,
                text: m.content.clone().unwrap_or_default(),
            }),
            _ => {}
        }
    }

    let app = App {
        log,
        cur_content: String::new(),
        cur_reasoning: String::new(),
        input: String::new(),
        input_cursor: 0,
        running: false,
        usage,
        current_cancel: None,
        tx_agent,
        rx_agent,
        model,
        phase,
        cost_per_1m,
        should_quit: false,
        switch_session: false,
        scroll: None,
        bottom_scroll: 0,
        pending_tools: Vec::new(),
    };

    let result = app_loop(&mut terminal, app).await;

    // 恢复终端
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, Show)?;
    result
}

async fn app_loop(terminal: &mut Terminal<CrosstermBackend<Stdout>>, mut app: App) -> Result<bool> {
    let mut key_events = EventStream::new();
    while !app.should_quit {
        render(terminal, &mut app)?;
        select! {
            Some(Ok(ev)) = key_events.next() => {
                if let Event::Key(k) = ev {
                    handle_key(&mut app, k);
                }
            }
            Some(ev) = app.rx_agent.recv() => {
                handle_agent_event(&mut app, ev);
            }
            else => break,
        }
    }
    Ok(app.switch_session)
}

fn handle_key(app: &mut App, k: KeyEvent) {
    match k.code {
        KeyCode::Enter if !app.running && k.modifiers.contains(KeyModifiers::CONTROL) => {
            let byte = char_to_byte(&app.input, app.input_cursor);
            app.input.insert(byte, '\n');
            app.input_cursor += 1;
        }
        KeyCode::Enter => {
            if !app.running && !app.input.trim().is_empty() {
                let text = app.input.clone();
                app.input.clear();
                app.input_cursor = 0;
                app.log.push(LogLine {
                    role: Role::User,
                    text: text.clone(),
                });
                app.running = true;
                app.cur_content.clear();
                app.cur_reasoning.clear();
                let cancel = CancellationToken::new();
                app.current_cancel = Some(cancel.clone());
                let _ = app.tx_agent.try_send((text, cancel));
            }
        }
        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(c) = &app.current_cancel {
                c.cancel();
            }
        }
        KeyCode::Char('q') if k.modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true;
        }
        KeyCode::Char('l') if k.modifiers.contains(KeyModifiers::CONTROL) => {
            // Ctrl-L：切回会话选择页（先取消正在进行的生成）
            if let Some(c) = &app.current_cancel {
                c.cancel();
            }
            app.switch_session = true;
            app.should_quit = true;
        }
        KeyCode::Up => {
            // 从底部位置开始往上滚，避免先跳到开头
            let n = app.scroll.unwrap_or(app.bottom_scroll);
            app.scroll = Some(n.saturating_sub(1));
        }
        KeyCode::Down => {
            let n = app.scroll.unwrap_or(app.bottom_scroll);
            app.scroll = Some(n + 1);
        }
        KeyCode::PageUp => {
            let n = app.scroll.unwrap_or(app.bottom_scroll);
            app.scroll = Some(n.saturating_sub(10));
        }
        KeyCode::PageDown => {
            let n = app.scroll.unwrap_or(app.bottom_scroll);
            app.scroll = Some(n + 10);
        }
        KeyCode::End => {
            app.scroll = None; // 回到底部
        }
        KeyCode::Char('a') if !app.running && k.modifiers.contains(KeyModifiers::CONTROL) => {
            app.input_cursor = 0;
        }
        KeyCode::Char('e') if !app.running && k.modifiers.contains(KeyModifiers::CONTROL) => {
            app.input_cursor = app.input.chars().count();
        }
        KeyCode::Left if !app.running => {
            app.input_cursor = app.input_cursor.saturating_sub(1);
        }
        KeyCode::Right if !app.running => {
            app.input_cursor = (app.input_cursor + 1).min(app.input.chars().count());
        }
        // Ctrl-J = raw 终端下的 Ctrl-Enter（ASCII 10），插入换行而非字符 j
        KeyCode::Char('j') if !app.running && k.modifiers.contains(KeyModifiers::CONTROL) => {
            let byte = char_to_byte(&app.input, app.input_cursor);
            app.input.insert(byte, '\n');
            app.input_cursor += 1;
        }
        KeyCode::Char(c) if !app.running => {
            let byte = char_to_byte(&app.input, app.input_cursor);
            app.input.insert(byte, c);
            app.input_cursor += 1;
        }
        KeyCode::Backspace if !app.running => {
            if app.input_cursor > 0 {
                let cur = char_to_byte(&app.input, app.input_cursor);
                let prev = char_to_byte(&app.input, app.input_cursor - 1);
                app.input.replace_range(prev..cur, "");
                app.input_cursor -= 1;
            }
        }
        KeyCode::Delete if !app.running => {
            let total = app.input.chars().count();
            if app.input_cursor < total {
                let cur = char_to_byte(&app.input, app.input_cursor);
                let next = char_to_byte(&app.input, app.input_cursor + 1);
                app.input.replace_range(cur..next, "");
            }
        }
        _ => {}
    }
}

/// char 索引转 byte 索引（越界返回 len）
fn char_to_byte(s: &str, ci: usize) -> usize {
    s.char_indices().nth(ci).map(|(b, _)| b).unwrap_or(s.len())
}

fn handle_agent_event(app: &mut App, ev: AgentEvent) {
    match ev {
        AgentEvent::Step { n } => {
            app.log.push(LogLine {
                role: Role::System,
                text: format!("— 思考中…（第 {} 次调用）—", n + 1),
            });
            app.scroll = None;
        }
        AgentEvent::Content(s) => {
            app.cur_content.push_str(&s);
            // 不重置 scroll：若处于跟随态(None)自然显示最新；用户手动上滚时不被打断
        }
        AgentEvent::Reasoning(s) => {
            app.cur_reasoning.push_str(&s);
            // 同上
        }
        AgentEvent::ToolCall { name, args } => {
            app.flush_cur();
            app.pending_tools.push(name.clone());
            // 参数截断展示：update_notes（整篇笔记）/generate_city_map（全量 POI JSON）等超长参数会污染聊天框
            let brief = if args.chars().count() > 80 {
                let s: String = args.chars().take(80).collect();
                format!("{s}…")
            } else {
                args
            };
            app.log.push(LogLine {
                role: Role::Tool,
                text: format!("→ {name}({brief})"),
            });
            app.scroll = None;
        }
        AgentEvent::ToolResult(r) => {
            // 与 ToolCall FIFO 配对，按工具决定展示详略：
            // 搜索类结果（几 KB~几十 KB）与记忆重写不在聊天框展开，只给一行提示
            let name = app.pending_tools.pop().unwrap_or_default();
            let text = match name.as_str() {
                "search_web" | "search_xhs" | "read_xhs_note" | "search_hotel_reviews" => {
                    format!("← {name} 完成（结果已供分析，聊天框不展示）")
                }
                "update_notes" => {
                    "← 会话记忆已更新（全文存 sessions/ 下本会话 .md 笔记，自动注入后续对话）"
                        .to_string()
                }
                _ => format!("← {r}"),
            };
            app.log.push(LogLine {
                role: Role::Tool,
                text,
            });
            app.scroll = None;
        }
        AgentEvent::Done(_) => {
            app.flush_cur();
            app.running = false;
            app.current_cancel = None;
            app.scroll = None;
        }
        AgentEvent::PhaseChange { phase } => {
            app.phase = phase;
        }
        AgentEvent::Usage(u) => {
            app.usage = u;
        }
        AgentEvent::Error(e) => {
            app.flush_cur();
            app.log.push(LogLine {
                role: Role::System,
                text: format!("[错误/取消] {e}"),
            });
            app.running = false;
            app.current_cancel = None;
            app.scroll = None;
        }
    }
}

impl App {
    fn flush_cur(&mut self) {
        if !self.cur_content.is_empty() {
            let t = std::mem::take(&mut self.cur_content);
            self.log.push(LogLine {
                role: Role::Assistant,
                text: t,
            });
        }
        self.cur_reasoning.clear();
    }
}

fn render(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> io::Result<()> {
    terminal.draw(|f| {
        let area = f.area();
        let inner_w = area.width.saturating_sub(2) as usize; // 减左右边框

        // 预算输入文本折行数：before + ▎ + after 按 inner_w 折行
        let prompt_str = if app.running {
            "(生成中，新输入已禁用)"
        } else {
            "输入消息，回车发送 / Ctrl-Enter 换行"
        };
        let before: String = app.input.chars().take(app.input_cursor).collect();
        let after: String = app.input.chars().skip(app.input_cursor).collect();
        let full_input = if app.running {
            format!("{before}{after}")
        } else {
            format!("{before}▎{after}")
        };
        // 先按 \n 拆成多行（Ctrl-Enter 输入的换行），每行再 wrap_lines 折行
        let raw_lines: Vec<Line> = full_input
            .split('\n')
            .map(|s| Line::from(Span::raw(s.to_string())))
            .collect();
        let input_wrapped = wrap_lines(&raw_lines, inner_w);
        let input_content_rows = input_wrapped.len().max(1);
        // prompt 1 行 + 输入内容行数 + 上下边框 2
        let input_h = (1 + input_content_rows + 2) as u16;

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),       // 状态栏
                Constraint::Min(5),          // 对话
                Constraint::Length(input_h), // 输入（随内容增长）
            ])
            .split(area);

        // 状态栏
        let cost = (app.usage.prompt_tokens as f64 * app.cost_per_1m.0
            + app.usage.completion_tokens as f64 * app.cost_per_1m.1)
            / 1_000_000.0;
        let status = Span::styled(
            format!(
                " 拾光者 | {} | {} | {} | tokens in·out·total = {}·{}·{} | ¥{:.4} ",
                app.model,
                phase_name(app.phase),
                if app.running {
                    "生成中…"
                } else {
                    "就绪"
                },
                app.usage.prompt_tokens,
                app.usage.completion_tokens,
                app.usage.total_tokens,
                cost
            ),
            Style::default().fg(Color::Cyan),
        );
        f.render_widget(
            Paragraph::new(Line::from(status)).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("拾光者 旅游规划 Agent"),
            ),
            chunks[0],
        );

        // 对话区：Paragraph + wrap（自动换行，长文本不溢出屏幕）
        let mut lines: Vec<Line> = Vec::new();
        for l in &app.log {
            lines.extend(LogLine::render(l));
        }
        if !app.cur_reasoning.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("💭 {}", app.cur_reasoning),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            )));
        }
        if !app.cur_content.is_empty() {
            let style = Style::default().fg(Color::White);
            let mut first = true;
            for part in app.cur_content.split('\n') {
                let mut spans: Vec<Span> = Vec::new();
                if first {
                    spans.push(Span::styled("拾光者: ", style.add_modifier(Modifier::BOLD)));
                    first = false;
                } else {
                    spans.push(Span::raw("  "));
                }
                spans.extend(md_line(part, style));
                lines.push(Line::from(spans));
            }
        }
        // 滚到底或手动滚动：用预折行后的行数算偏移（不再用 Paragraph::wrap，避免二次折行导致 scroll 失准）
        let chat_height = chunks[1].height.saturating_sub(2) as usize; // 减边框
        let wrapped = wrap_lines(&lines, chunks[1].width.saturating_sub(2) as usize);
        let total = wrapped.len();
        let bottom = total.saturating_sub(chat_height);
        app.bottom_scroll = bottom; // 供按键从底部开始上滚
        let scroll = match app.scroll {
            Some(n) => n.min(total.saturating_sub(1)),
            None => bottom, // 跟随底部
        };
        let chat = Paragraph::new(Text::from_iter(wrapped))
            .scroll((scroll as u16, 0))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(if app.scroll.is_some() {
                        "对话（手动滚动中，End 回到底部）"
                    } else {
                        "对话（Ctrl-C 取消 / Ctrl-L 切会话 / Ctrl-Q 退出）"
                    }),
            );
        f.render_widget(chat, chunks[1]);

        // 输入框：prompt 独立第一行，输入文本（含 ▎）折行后依次渲染
        let mut input_lines: Vec<Line> = vec![Line::from(Span::styled(
            format!("{prompt_str}> "),
            Style::default().fg(Color::Yellow),
        ))];
        input_lines.extend(input_wrapped);
        f.render_widget(
            Paragraph::new(input_lines).block(Block::default().borders(Borders::ALL)),
            chunks[2],
        );
    })?;
    Ok(())
}

impl LogLine {
    /// 按 \n 拆成多行；首行带角色标签，续行无标签（缩进对齐）。
    fn render(l: &LogLine) -> Vec<Line<'static>> {
        let (tag, style) = match l.role {
            Role::User => ("你", Style::default().fg(Color::Green)),
            Role::Assistant => ("拾光者", Style::default().fg(Color::White)),
            Role::Tool => ("工具", Style::default().fg(Color::Magenta)),
            Role::System => ("系统", Style::default().fg(Color::DarkGray)),
        };
        let tag_span = Span::styled(format!("{tag}: "), style.add_modifier(Modifier::BOLD));
        // 仅对用户/助手内容做 markdown 渲染；工具与系统保持纯文本
        let use_md = matches!(l.role, Role::Assistant | Role::User);
        let mut out: Vec<Line<'static>> = Vec::new();
        let mut first = true;
        for part in l.text.split('\n') {
            let mut spans: Vec<Span> = Vec::new();
            if first {
                spans.push(tag_span.clone());
                first = false;
            } else {
                // 续行缩进，对齐标签宽度（简化为两空格）
                spans.push(Span::raw("  "));
            }
            if use_md {
                spans.extend(md_line(part, style));
            } else {
                spans.push(Span::styled(part.to_string(), style));
            }
            out.push(Line::from(spans));
        }
        if out.is_empty() {
            out.push(Line::from(if first {
                vec![tag_span, Span::styled(String::new(), style)]
            } else {
                vec![Span::raw("  "), Span::styled(String::new(), style)]
            }));
        }
        out
    }
}

/// 行级 markdown：标题 / 引用 / 列表 / 代码块标记，其余走 inline。
fn md_line(line: &str, base: Style) -> Vec<Span<'static>> {
    // 标题（长前缀优先）
    if let Some(rest) = line.strip_prefix("### ") {
        return vec![Span::styled(
            rest.to_string(),
            base.add_modifier(Modifier::BOLD),
        )];
    }
    if let Some(rest) = line.strip_prefix("## ") {
        return vec![Span::styled(
            rest.to_string(),
            base.fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )];
    }
    if let Some(rest) = line.strip_prefix("# ") {
        return vec![Span::styled(
            rest.to_string(),
            base.fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )];
    }
    // 引用
    if let Some(rest) = line.strip_prefix("> ") {
        let mut spans = vec![Span::raw("  ")];
        spans.extend(md_inline(rest, base.add_modifier(Modifier::DIM)));
        return spans;
    }
    // 无序列表（- 或 *，但排除 ** 粗体开头）
    if let Some(rest) = line.strip_prefix("- ") {
        let mut spans = vec![Span::styled("• ", base.fg(Color::Cyan))];
        spans.extend(md_inline(rest, base));
        return spans;
    }
    if line.starts_with("* ") && !line.starts_with("** ") {
        let rest = &line[2..];
        let mut spans = vec![Span::styled("• ", base.fg(Color::Cyan))];
        spans.extend(md_inline(rest, base));
        return spans;
    }
    // 代码块围栏标记
    if line.trim_start().starts_with("```") {
        return vec![Span::styled(line.to_string(), base.fg(Color::DarkGray))];
    }
    // markdown 表格行：暂时关闭（渲染效果不佳，表格行按普通文本处理）
    // if is_table_row(line) {
    //     return render_table_row(line, base);
    // }
    md_inline(line, base)
}

/// 把当前缓冲区落成一个带样式的 span。
fn flush_span(
    buf: &mut String,
    spans: &mut Vec<Span>,
    base: Style,
    bold: bool,
    italic: bool,
    code: bool,
) {
    if buf.is_empty() {
        return;
    }
    let mut st = base;
    if bold {
        st = st.add_modifier(Modifier::BOLD);
    }
    if italic {
        st = st.add_modifier(Modifier::ITALIC);
    }
    if code {
        st = st.fg(Color::Yellow);
    }
    spans.push(Span::styled(std::mem::take(buf), st));
}

/// inline markdown：`**粗体**`、`*斜体*`/`_斜体_`、`` `代码` ``。
fn md_inline(text: &str, base: Style) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut bold = false;
    let mut italic = false;
    let mut code = false;
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '`' {
            flush_span(&mut buf, &mut spans, base, bold, italic, code);
            code = !code;
            i += 1;
            continue;
        }
        if c == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            flush_span(&mut buf, &mut spans, base, bold, italic, code);
            bold = !bold;
            i += 2;
            continue;
        }
        if c == '*' || c == '_' {
            flush_span(&mut buf, &mut spans, base, bold, italic, code);
            italic = !italic;
            i += 1;
            continue;
        }
        buf.push(c);
        i += 1;
    }
    flush_span(&mut buf, &mut spans, base, bold, italic, code);
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base));
    }
    spans
}

/// 按显示宽度预折行，返回可用于 Paragraph 的行列表（同时让 scroll 能算到底部偏移）。
fn wrap_lines(lines: &[Line], width: usize) -> Vec<Line<'static>> {
    use unicode_width::UnicodeWidthStr;
    let mut out: Vec<Line> = Vec::new();
    if width == 0 {
        return Vec::new();
    }
    for line in lines {
        // 把 Line 的 spans 展平成 (字符串, 样式) 段
        let mut chunks: Vec<(String, Style)> = Vec::new();
        // Line::spans 在 ratatui 0.29 是字段
        for sp in &line.spans {
            chunks.push((sp.content.to_string(), sp.style));
        }
        let mut cur_spans: Vec<Span> = Vec::new();
        let mut cur_w: usize = 0;
        let push_span = |cur: &mut Vec<Span>, w: &mut usize, s: String, st: Style| {
            if s.is_empty() {
                return;
            }
            *w += UnicodeWidthStr::width(s.as_str());
            cur.push(Span::styled(s, st));
        };
        for (text, st) in chunks {
            // 逐字符判断是否需要断行
            let mut buf = String::new();
            for ch in text.chars() {
                let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                if cur_w + cw > width && !buf.is_empty() {
                    // flush buf
                    let s = std::mem::take(&mut buf);
                    push_span(&mut cur_spans, &mut cur_w, s, st);
                    // 断行
                    out.push(std::mem::take(&mut cur_spans).into());
                    cur_w = 0;
                }
                buf.push(ch);
                cur_w += cw;
                if cur_w >= width {
                    let s = std::mem::take(&mut buf);
                    push_span(&mut cur_spans, &mut cur_w, s, st);
                    out.push(std::mem::take(&mut cur_spans).into());
                    cur_w = 0;
                }
            }
            if !buf.is_empty() {
                let s = std::mem::take(&mut buf);
                push_span(&mut cur_spans, &mut cur_w, s, st);
            }
        }
        if !cur_spans.is_empty() {
            out.push(std::mem::take(&mut cur_spans).into());
        }
    }
    out
}

/// 判断是否为 markdown 表格行：含 |，且（以 | 起或收，或单元格数≥2）。
#[allow(dead_code)]
fn is_table_row(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() || !t.contains('|') {
        return false;
    }
    // 分隔行 |---|---| 单独识别（按 ─ 渲染）
    let cells: Vec<&str> = t.split('|').filter(|s| !s.trim().is_empty()).collect();
    // 排除：单独的列表/引用已先处理；这里只要 | 数 ≥ 2 视为表格
    cells.len() >= 2
}

/// 渲染一行表格：按 | 切分，单元格用 │ 分隔；分隔行（全 -）画 ─。
#[allow(dead_code)]
fn render_table_row(line: &str, base: Style) -> Vec<Span<'static>> {
    let t = line.trim();
    let inner = t.trim_matches('|');
    let cells: Vec<&str> = inner.split('|').map(|s| s.trim()).collect();
    // 分隔行：单元格全是 - / : / 空格
    let is_sep = cells
        .iter()
        .all(|c| !c.is_empty() && c.chars().all(|ch| ch == '-' || ch == ':' || ch == ' '));
    if is_sep {
        // 画一条横线
        let line_str = "─".repeat(inner.chars().count());
        return vec![Span::styled(line_str, base.fg(Color::DarkGray))];
    }
    let mut spans: Vec<Span> = Vec::new();
    let sep = Span::styled("│ ", base.fg(Color::DarkGray));
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            spans.push(sep.clone());
        }
        spans.push(Span::styled(format!("{cell} "), base));
    }
    spans
}

/// 阶段号 → 简称
fn phase_name(phase: u8) -> String {
    let names = [
        "阶段0·种草闲聊",
        "阶段1·信息采集",
        "阶段2·大局规划",
        "阶段3·逐part确定",
        "阶段4·整体调整",
        "阶段5·完整攻略",
    ];
    names
        .get(phase as usize)
        .map(|s| s.to_string())
        .unwrap_or_else(|| "阶段5·完整攻略".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app(input: &str, cursor: usize) -> App {
        let (tx_agent, _rx_input) = tokio::sync::mpsc::channel(1);
        let (_tx_event, rx_agent) = tokio::sync::mpsc::channel(1);
        App {
            log: Vec::new(),
            cur_content: String::new(),
            cur_reasoning: String::new(),
            input: input.to_string(),
            input_cursor: cursor,
            running: false,
            usage: Usage::default(),
            current_cancel: None,
            tx_agent,
            rx_agent,
            model: "test-model".into(),
            phase: 0,
            cost_per_1m: (0.0, 0.0),
            should_quit: false,
            switch_session: false,
            scroll: None,
            bottom_scroll: 0,
            pending_tools: Vec::new(),
        }
    }

    #[test]
    fn ctrl_enter_inserts_newline_at_cursor() {
        let mut app = test_app("甲乙", 1);
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
        );
        assert_eq!(app.input, "甲\n乙");
        assert_eq!(app.input_cursor, 2);
        assert!(!app.running);
    }
}
