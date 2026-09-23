//! 小红书内容工具（search_xhs / read_xhs_note）：经本机 xiaohongshu-mcp 服务读取。
//! 手写 MCP StreamableHTTP 客户端（JSON-RPC 2.0）：initialize 握手（捕获 Mcp-Session-Id）
//! → tools/call；响应兼容 application/json 与 text/event-stream 两种格式。
//! 只读为主：搜笔记 + 读笔记详情与评论；唯一写操作是精读后 LIKE_PROBABILITY_PCT 概率的
//! 随手点赞（2026-09-10 用户决策），发布/评论/关注等其余写操作一律不接。
//! 拟人防风控：调用间随机变速限流；搜索只透传头部几篇；评论默认随机读头部几条。
//! 服务未启动/未登录时返回清晰提示，不阻塞对话（同「地图失败不阻塞」纪律）。

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

use super::Tool;

/// 精读笔记后随机点赞的概率（%）。唯一放行的写操作，纯随机（用户决策）。
const LIKE_PROBABILITY_PCT: u64 = 5;

/// 拟人限流器：相邻 MCP 调用之间强制随机间隔（min + rand(0..jitter)）。
/// 目的：小红书风控对「匀速高频机器节奏」敏感，把 agent 的连环调用拉成人的变速节奏。
/// xorshift64 无需引入 rand 依赖；纳秒种子的抖动对本用途足够。
#[derive(Clone)]
struct XhsRateLimiter {
    state: Arc<tokio::sync::Mutex<LimiterState>>,
}

struct LimiterState {
    last: Option<std::time::Instant>,
    rng: u64,
}

impl XhsRateLimiter {
    fn new() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E3779B97F4A7C15)
            | 1;
        Self {
            state: Arc::new(tokio::sync::Mutex::new(LimiterState {
                last: None,
                rng: seed,
            })),
        }
    }

    fn next_rand(&self, st: &mut LimiterState) -> u64 {
        // xorshift64*
        let mut x = st.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        st.rng = x;
        x
    }

    /// 等待到本次调用允许发出。min_ms/jitter_ms 来自 config.toml [xhs]。
    async fn wait(&self, min_ms: u64, jitter_ms: u64) {
        let mut st = self.state.lock().await;
        let jitter = if jitter_ms == 0 {
            0
        } else {
            self.next_rand(&mut st) % jitter_ms
        };
        let required = Duration::from_millis(min_ms.saturating_add(jitter));
        let sleep = match st.last {
            Some(last) => last.elapsed().saturating_sub(required),
            None => Duration::ZERO,
        };
        if !sleep.is_zero() {
            tokio::time::sleep(sleep).await;
        }
        st.last = Some(std::time::Instant::now());
    }
}

/// 极简 MCP StreamableHTTP 客户端。
#[derive(Clone)]
pub struct XhsClient {
    http: reqwest::Client,
    url: String,
    token: Option<String>,
    session: Arc<tokio::sync::Mutex<Option<String>>>,
    limiter: XhsRateLimiter,
    min_interval_ms: u64,
    jitter_ms: u64,
}

impl XhsClient {
    pub fn new(url: String, token: Option<String>, min_interval_ms: u64, jitter_ms: u64) -> Self {
        // xiaohongshu-mcp 靠浏览器自动化干活，单次调用可能 10-60s（加载全部评论更久）：
        // 禁总超时；read_timeout 为空闲检测（浏览器翻页/滚动期间可能长时间无响应）
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(180))
            .build()
            .expect("构建 HTTP 客户端失败");
        Self {
            http,
            url,
            token,
            session: Arc::new(tokio::sync::Mutex::new(None)),
            limiter: XhsRateLimiter::new(),
            min_interval_ms,
            jitter_ms,
        }
    }

    async fn post_rpc(&self, body: &Value) -> Result<reqwest::Response> {
        let mut req = self
            .http
            .post(&self.url)
            .header("Content-Type", "application/json")
            // MCP StreamableHTTP 规范要求 Accept 同时带两种类型
            .header("Accept", "application/json, text/event-stream");
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        if let Some(sid) = self.session.lock().await.clone() {
            req = req.header("Mcp-Session-Id", sid);
        }
        let resp = req
            .json(body)
            .send()
            .await
            .with_context(|| format!("请求 xiaohongshu-mcp 失败（{url}）", url = self.url))?;
        Ok(resp)
    }

    /// initialize 握手：捕获 Mcp-Session-Id（服务有状态时返回），并回 notifications/initialized。
    async fn ensure_session(&self) -> Result<()> {
        if self.session.lock().await.is_some() {
            return Ok(());
        }
        let body = json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "cheaptrip", "version": "1.0.0"}
            }
        });
        let resp = self.post_rpc(&body).await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "MCP initialize 失败 {status}: {}",
                truncate(&text, 300)
            ));
        }
        if let Some(sid) = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            *self.session.lock().await = Some(sid.to_string());
        }
        // 有状态服务要求握手完成通知（无 id）；无状态服务忽略之
        let notify = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        let _ = self.post_rpc(&notify).await;
        Ok(())
    }

    /// 调 MCP 工具，返回拼接后的文本内容。session 失效（服务重启）自动重握手重试一次。
    /// 每次 tools/call 前过拟人限流器（随机变速间隔）。
    pub async fn call_tool(&self, name: &str, args: Value) -> Result<String> {
        let body = self.call_tool_raw(name, args).await?;
        extract_tool_text(&body)
    }

    /// 调用小红书登录状态工具。
    pub async fn check_login_status(&self) -> Result<String> {
        self.call_tool("check_login_status", json!({})).await
    }

    /// 调用小红书登出工具。
    pub async fn delete_cookies(&self) -> Result<String> {
        self.call_tool("delete_cookies", json!({})).await
    }

    /// 获取登录二维码原始图片。二维码只在内存中短暂保存，由 Web 管理接口下发。
    pub async fn get_login_qrcode(&self) -> Result<XhsLoginQr> {
        let body = self.call_tool_raw("get_login_qrcode", json!({})).await?;
        parse_login_qrcode(&body)
    }

    async fn call_tool_raw(&self, name: &str, args: Value) -> Result<String> {
        self.ensure_session().await?;
        self.limiter
            .wait(self.min_interval_ms, self.jitter_ms)
            .await;
        let body = json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": name, "arguments": args}
        });
        let resp = self.post_rpc(&body).await?;
        let status = resp.status();
        if !status.is_success() && matches!(status.as_u16(), 400 | 404) {
            // 多半是 session 失效：清掉重握手再来一次
            *self.session.lock().await = None;
            self.ensure_session().await?;
            let resp = self.post_rpc(&body).await?;
            return self.rpc_body(resp).await;
        }
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "MCP tools/call 失败 {status}: {}",
                truncate(&text, 300)
            ));
        }
        self.rpc_body(resp).await
    }

    /// 拟人随机数 [lo, hi]（闭区间），与限流器共用熵源。
    pub async fn rand_range(&self, lo: u64, hi: u64) -> u64 {
        let mut st = self.limiter.state.lock().await;
        let span = hi.saturating_sub(lo) + 1;
        lo + if span == 0 {
            0
        } else {
            self.limiter.next_rand(&mut st) % span
        }
    }

    async fn rpc_body(&self, resp: reqwest::Response) -> Result<String> {
        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = resp.text().await.context("读取 xiaohongshu-mcp 响应失败")?;
        let json_str = if ct.contains("text/event-stream") {
            sse_payload(&body)?
        } else {
            body
        };
        Ok(json_str)
    }
}

/// 小红书登录二维码响应；data 为不带 data URI 前缀的 Base64。
#[derive(Debug, Clone)]
pub struct XhsLoginQr {
    pub data: String,
    pub mime_type: String,
    pub text: Option<String>,
}

/// 从 SSE 文本抽最后一条 data: JSON 帧（MCP StreamableHTTP 可能以 SSE 包裹响应）。
fn sse_payload(body: &str) -> Result<String> {
    let mut last: Option<&str> = None;
    for line in body.lines() {
        if let Some(d) = line.strip_prefix("data:") {
            let d = d.trim();
            if !d.is_empty() {
                last = Some(d);
            }
        }
    }
    last.map(str::to_string)
        .ok_or_else(|| anyhow!("SSE 响应中没有 data 帧: {}", truncate(body, 300)))
}

/// 解析 JSON-RPC 响应：取 result.content[].text 拼接；error 对象或 isError=true → 报错。
fn extract_tool_text(json_str: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct Rpc {
        #[serde(default)]
        result: Option<RpcResult>,
        #[serde(default)]
        error: Option<RpcError>,
    }
    #[derive(Deserialize)]
    struct RpcResult {
        #[serde(default)]
        content: Vec<RpcContent>,
        #[serde(default, rename = "isError")]
        is_error: bool,
    }
    #[derive(Deserialize)]
    struct RpcContent {
        #[serde(default)]
        text: String,
    }
    #[derive(Deserialize)]
    struct RpcError {
        #[serde(default)]
        message: String,
    }

    let rpc: Rpc = serde_json::from_str(json_str)
        .with_context(|| format!("解析 MCP 响应失败: {}", truncate(json_str, 300)))?;
    if let Some(e) = rpc.error {
        return Err(anyhow!("xiaohongshu-mcp 错误: {}", e.message));
    }
    let Some(r) = rpc.result else {
        return Err(anyhow!("MCP 响应缺 result: {}", truncate(json_str, 300)));
    };
    let text = r
        .content
        .iter()
        .map(|c| c.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if r.is_error {
        return Err(anyhow!(
            "小红书工具执行失败：{}（常见原因：服务未扫码登录 / xsec_token 过期，重新 search 一次即可）",
            truncate(&text, 300)
        ));
    }
    if text.is_empty() {
        return Err(anyhow!("xiaohongshu-mcp 返回空内容"));
    }
    Ok(text)
}

fn parse_login_qrcode(json_str: &str) -> Result<XhsLoginQr> {
    #[derive(Deserialize)]
    struct Rpc {
        #[serde(default)]
        result: Option<RpcResult>,
        #[serde(default)]
        error: Option<RpcError>,
    }
    #[derive(Deserialize)]
    struct RpcResult {
        #[serde(default)]
        content: Vec<RpcContent>,
        #[serde(default, rename = "isError")]
        is_error: bool,
    }
    #[derive(Deserialize)]
    struct RpcContent {
        #[serde(rename = "type")]
        kind: String,
        #[serde(default)]
        data: String,
        #[serde(default, rename = "mimeType")]
        mime_type: String,
        #[serde(default)]
        text: String,
    }
    #[derive(Deserialize)]
    struct RpcError {
        #[serde(default)]
        message: String,
    }

    let rpc: Rpc = serde_json::from_str(json_str)
        .with_context(|| format!("解析小红书二维码响应失败：{}", truncate(json_str, 300)))?;
    if let Some(error) = rpc.error {
        return Err(anyhow!("xiaohongshu-mcp 错误：{}", error.message));
    }
    let result = rpc.result.ok_or_else(|| anyhow!("二维码响应缺少 result"))?;
    if result.is_error {
        return Err(anyhow!("小红书二维码获取失败"));
    }
    let image = result
        .content
        .iter()
        .find(|content| content.kind == "image" && !content.data.trim().is_empty())
        .ok_or_else(|| anyhow!("小红书响应中没有二维码图片"))?;
    let (mime_type, data) = image
        .data
        .split_once(",")
        .filter(|(prefix, _)| prefix.starts_with("data:"))
        .map_or_else(
            || (image.mime_type.clone(), image.data.clone()),
            |(prefix, data)| {
                (
                    prefix
                        .strip_prefix("data:")
                        .and_then(|value| value.split(';').next())
                        .unwrap_or("image/png")
                        .to_owned(),
                    data.to_owned(),
                )
            },
        );
    Ok(XhsLoginQr {
        data,
        mime_type: if mime_type.is_empty() {
            "image/png".into()
        } else {
            mime_type
        },
        text: result
            .content
            .iter()
            .find(|content| content.kind == "text" && !content.text.is_empty())
            .map(|content| content.text.clone()),
    })
}

/// 输出截断保护（防超长结果撑爆上下文），尾部加省略标记。
fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let t: String = s.chars().take(max_chars).collect();
        format!("{t}\n…（内容过长已截断）")
    }
}

/// 统一的友好错误包装：给出排查提示，LLM 可据此降级到 search_web。
fn friendly(e: anyhow::Error) -> anyhow::Error {
    anyhow!(
        "小红书工具失败：{e:#}\n（提示：需本机 xiaohongshu-mcp 服务运行且已扫码登录；\
         服务不可用属正常情况，改用 search_web 继续即可，不阻塞当前工作）"
    )
}

// ---- 工具一：搜索小红书笔记 ----

pub struct SearchXhs {
    client: XhsClient,
}

impl SearchXhs {
    pub fn new(client: XhsClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for SearchXhs {
    fn name(&self) -> &str {
        "search_xhs"
    }
    fn description(&self) -> &str {
        "搜索小红书笔记（真实游客笔记：攻略/避坑/体验/美食）。\
         返回笔记列表，其中 feed_id 与 xsec_token 是 read_xhs_note 读详情的必要参数。\
         需要本机 xiaohongshu-mcp 服务并已扫码登录。"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "keyword": {"type": "string", "description": "搜索关键词，如「陕北 攻略 路线」「壶口瀑布 避坑」"},
                "sort_by": {"type": "string", "description": "可选排序：综合（默认）|最新|最多点赞|最多评论|最多收藏"},
                "note_type": {"type": "string", "description": "可选笔记类型：不限（默认）|视频|图文"},
                "publish_time": {"type": "string", "description": "可选发布时间：不限（默认）|一天内|一周内|半年内"}
            },
            "required": ["keyword"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String> {
        let keyword = args
            .get("keyword")
            .and_then(|v| v.as_str())
            .context("search_xhs 缺少 keyword 参数")?;
        let mut filters = serde_json::Map::new();
        for key in ["sort_by", "note_type", "publish_time"] {
            if let Some(v) = args.get(key).and_then(|v| v.as_str()) {
                filters.insert(key.into(), json!(v));
            }
        }
        let mut payload = json!({"keyword": keyword});
        if !filters.is_empty() {
            payload["filters"] = Value::Object(filters);
        }
        let text = self
            .client
            .call_tool("search_feeds", payload)
            .await
            .map_err(friendly)?;
        // 拟人截取：一次搜索只细看头部随机 3~5 篇（服务端返回更多也只透传给 LLM 这几条）
        let keep = self.client.rand_range(3, 5).await as usize;
        Ok(truncate(&limit_feeds(&text, keep), 16000))
    }
}

/// 搜索结果截取：feeds 数组只保留前 n 条。
/// 非 JSON / 缺 feeds 字段时原样返回（MCP 输出格式变化时的兜底）。
fn limit_feeds(text: &str, n: usize) -> String {
    let Ok(mut v) = serde_json::from_str::<Value>(text) else {
        return text.to_string();
    };
    let Some(feeds) = v.get_mut("feeds").and_then(|f| f.as_array_mut()) else {
        return text.to_string();
    };
    feeds.truncate(n);
    serde_json::to_string_pretty(&v).unwrap_or_else(|_| text.to_string())
}

// ---- 工具二：读取笔记详情与评论 ----

pub struct ReadXhsNote {
    client: XhsClient,
}

impl ReadXhsNote {
    pub fn new(client: XhsClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for ReadXhsNote {
    fn name(&self) -> &str {
        "read_xhs_note"
    }
    fn description(&self) -> &str {
        "读取小红书笔记详情：正文、互动数据（点赞/收藏/评论数）、评论列表及子评论。\
         feed_id 与 xsec_token 必须来自 search_xhs 的返回结果。评论区是避坑与时效信息的金矿。\
         每次调用自动随机减速（拟人限流），评论默认只随机读取头部 3~8 条。"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "feed_id": {"type": "string", "description": "笔记 ID（search_xhs 结果中获取）"},
                "xsec_token": {"type": "string", "description": "访问令牌（search_xhs 结果中获取，与 feed_id 配对）"},
                "load_all_comments": {"type": "boolean", "description": "可选，深挖模式（默认按系统拟人策略随机限制条数）；仅在评论区确有深挖价值时开启"},
                "limit": {"type": "integer", "description": "可选，一级评论数量上限（默认随机 3~8，封顶 10）"}
            },
            "required": ["feed_id", "xsec_token"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String> {
        let feed_id = args
            .get("feed_id")
            .and_then(|v| v.as_str())
            .context("read_xhs_note 缺少 feed_id 参数")?;
        let xsec_token = args
            .get("xsec_token")
            .and_then(|v| v.as_str())
            .context("read_xhs_note 缺少 xsec_token 参数（从 search_xhs 结果获取）")?;
        // 拟人评论策略：只看头部随机 3~8 条（没人逐条翻完评论区）；
        // LLM 显式要深挖时尊重其意图，但一级评论封顶 10 条。
        let (load_all, limit) = match (
            args.get("load_all_comments").and_then(|v| v.as_bool()),
            args.get("limit").and_then(|v| v.as_u64()),
        ) {
            (Some(true), Some(l)) => (true, l.min(10)),
            (Some(true), None) => (true, 10),
            (Some(false), _) => (false, 0),
            (None, Some(l)) => (true, l.min(10)),
            (None, None) => (true, self.client.rand_range(3, 8).await),
        };
        let mut payload = json!({
            "feed_id": feed_id,
            "xsec_token": xsec_token,
            "load_all_comments": load_all
        });
        if load_all {
            payload["limit"] = json!(limit);
        }
        let text = self
            .client
            .call_tool("get_feed_detail", payload)
            .await
            .map_err(friendly)?;
        let mut out = truncate(&text, 24000);
        // 拟人行为：精读完成后小概率顺手点赞（2026-09-10 用户决策接入的唯一写操作）。
        // like_feed 幂等（已赞自动跳过）；失败不传播，不影响阅读结果。
        if self.client.rand_range(1, 100).await <= LIKE_PROBABILITY_PCT {
            let like = self
                .client
                .call_tool(
                    "like_feed",
                    json!({"feed_id": feed_id, "xsec_token": xsec_token}),
                )
                .await;
            match like {
                Ok(_) => out.push_str("\n\n（已顺手为该笔记点赞）"),
                Err(e) => out.push_str(&format!("\n\n（顺手点赞未成功，不影响阅读：{e:#}）")),
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SSE 包裹的响应：抽最后一条 data 帧。
    #[test]
    fn sse_payload_extracts_last_data() {
        let body = "event: message\ndata: {\"a\":1}\n\nevent: message\ndata: {\"b\":2}\n";
        assert_eq!(sse_payload(body).unwrap(), "{\"b\":2}");
        assert!(sse_payload("no data here").is_err());
    }

    /// JSON-RPC 解析：正常 result / isError / error 对象 / 空内容。
    #[test]
    fn extract_tool_text_cases() {
        let ok = r#"{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"笔记A"},{"type":"text","text":"笔记B"}],"isError":false}}"#;
        assert_eq!(extract_tool_text(ok).unwrap(), "笔记A\n笔记B");
        let is_err = r#"{"result":{"content":[{"type":"text","text":"未登录"}],"isError":true}}"#;
        assert!(extract_tool_text(is_err).is_err());
        let rpc_err = r#"{"error":{"code":-32602,"message":"bad args"}}"#;
        assert!(
            extract_tool_text(rpc_err)
                .unwrap_err()
                .to_string()
                .contains("bad args")
        );
        let empty = r#"{"result":{"content":[]}}"#;
        assert!(extract_tool_text(empty).is_err());
        assert!(extract_tool_text("not json").is_err());
    }

    /// 截断保护。
    #[test]
    fn truncate_caps_length() {
        let s = "好".repeat(200);
        let t = truncate(&s, 50);
        assert!(t.chars().count() < 200 && t.contains("已截断"));
        assert_eq!(truncate("短", 10), "短");
    }

    /// 搜索结果截取：feeds 只留前 n 条且字段原样；非 JSON/缺 feeds 原样返回。
    #[test]
    fn limit_feeds_truncates_and_passthrough() {
        let mk = |n: usize| {
            let feeds: Vec<Value> = (0..n)
                .map(|i| json!({"feedId": format!("id{i}"), "xsecToken": format!("t{i}")}))
                .collect();
            json!({"feeds": feeds}).to_string()
        };
        let out = limit_feeds(&mk(8), 4);
        let v: Value = serde_json::from_str(&out).unwrap();
        let arr = v["feeds"].as_array().unwrap();
        assert_eq!(arr.len(), 4);
        assert_eq!(arr[0]["feedId"], "id0");
        assert_eq!(arr[3]["xsecToken"], "t3");
        // 少于 n：原样保留
        let v2: Value = serde_json::from_str(&limit_feeds(&mk(2), 4)).unwrap();
        assert_eq!(v2["feeds"].as_array().unwrap().len(), 2);
        // 非 JSON / 缺 feeds：原样透传
        assert_eq!(limit_feeds("不是json", 3), "不是json");
        assert_eq!(limit_feeds("{\"other\":1}", 3), "{\"other\":1}");
    }

    #[test]
    fn login_qrcode_parses_image_content() {
        let body = r#"{"result":{"content":[{"type":"text","text":"请扫码"},{"type":"image","mimeType":"image/png","data":"data:image/png;base64,QUJD"}]}}"#;
        let qr = parse_login_qrcode(body).unwrap();
        assert_eq!(qr.mime_type, "image/png");
        assert_eq!(qr.data, "QUJD");
        assert_eq!(qr.text.as_deref(), Some("请扫码"));
    }

    /// 实跑：搜索小红书（需本机 xiaohongshu-mcp 服务运行且已扫码登录）。
    /// 运行：cargo test xhs_search_live -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn xhs_search_live() {
        let _ = dotenvy::dotenv();
        let url =
            std::env::var("XHS_MCP_URL").unwrap_or_else(|_| "http://localhost:18060/mcp".into());
        let token = std::env::var("XHS_MCP_TOKEN")
            .ok()
            .filter(|s| !s.is_empty());
        let tool = SearchXhs::new(XhsClient::new(url, token, 8000, 7000));
        let out = tool
            .execute(serde_json::json!({"keyword": "壶口瀑布 攻略", "sort_by": "最多点赞"}))
            .await
            .expect("execute 失败");
        println!("\n=== search_xhs 输出 ===\n{out}\n");
        assert!(!out.is_empty());
    }
}
