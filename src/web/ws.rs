//! WS：`/ws/{sid}` —— 连上先补历史，之后逐条推 AgentEvent；
//! 客户端→服务端：`{"type":"chat","text":...}` / `{"type":"stop"}`。
//!
//! AgentEvent 本体零改动（Serialize 形状原样），外层包一层
//! `{"type":"event","event":...}` 便于前端与 history/error 等控制消息区分。

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Extension, Path, State, WebSocketUpgrade};
use axum::response::Response;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast;

use super::auth::repository::UserRecord;
use super::rest::{ApiError, SessionDetailDto, load_owned_session};
use super::state::{CoreHandle, SharedState};
use crate::session::Session;

/// GET /ws/{sid}：升级请求同样过口令鉴权中间件（浏览器 WS 无法带 header，走 ?token=）。
pub async fn ws_handler(
    State(st): State<SharedState>,
    Path(sid): Path<String>,
    ws: WebSocketUpgrade,
    owner: Option<Extension<UserRecord>>,
) -> Result<Response, ApiError> {
    let _ = load_owned_session(&st, &sid, owner.as_ref())?;
    Ok(ws.on_upgrade(move |socket| ws_session(st, sid, socket)))
}

async fn ws_session(st: SharedState, sid: String, socket: WebSocket) {
    let handle = match st.ensure_core(&sid) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!("[ws:{sid}] 启动会话失败：{e:#}");
            return;
        }
    };
    let (mut sink, mut stream) = socket.split();

    // 先订阅再补历史：订阅点之后的增量不丢（历史只含已存档轮次，流式中的
    // 增量重连后拿不到是 M0 已知限制——一轮结束 Session 落盘后刷新即齐）
    let mut events = handle.events.subscribe();

    // 1. 补历史（含开场白/全部存档消息 + phase + usage）
    let history = match Session::load(&sid) {
        Ok(s) => json!({"type": "history", "data": SessionDetailDto::from(s)}),
        Err(e) => json!({"type": "error", "message": format!("读取历史失败：{e:#}")}),
    };
    if !send_json(&mut sink, history).await {
        return;
    }

    // 2. 双向循环：服务端事件扇出 ↓；客户端 chat/stop ↑
    loop {
        tokio::select! {
            biased; // 事件推送优先于新输入
            ev = events.recv() => {
                match ev {
                    Ok(ev) => {
                        let msg = json!({"type": "event", "event": ev});
                        if !send_json(&mut sink, msg).await {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // 慢消费者丢帧：明确告知前端刷新取历史
                        let msg = json!({"type": "lagged", "dropped": n});
                        if !send_json(&mut sink, msg).await {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            msg = stream.next() => {
                match msg {
                    Some(Ok(Message::Text(t))) => {
                        handle_client_msg(st.as_ref(), &handle, &t, &mut sink).await;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {} // ping/pong/binary 忽略
                    Some(Err(e)) => {
                        tracing::debug!("[ws:{sid}] 连接收发错误：{e}");
                        break;
                    }
                }
            }
        }
    }
}

/// 处理一条客户端文本消息（chat/stop；非法与空文本静默忽略）。
async fn handle_client_msg(
    st: &super::state::AppState,
    handle: &CoreHandle,
    raw: &str,
    sink: &mut futures::stream::SplitSink<WebSocket, Message>,
) {
    match parse_client_msg(raw) {
        ClientMsg::Chat(text) => {
            // 本会话忙：直接拒绝，不去排全局闸门的队（chat 内还有兜底防竞态）
            if handle.is_busy() {
                let msg = json!({"type": "error", "message": "该会话已有对话在进行中"});
                let _ = send_json(sink, msg).await;
                return;
            }
            // 全局并发闸门：满员在此排队（第 3 个会话等前面轮次结束）
            match st.gate.clone().acquire_owned().await {
                Ok(permit) => {
                    if let Err(e) = handle.chat(text, permit) {
                        let msg = json!({"type": "error", "message": format!("{e:#}")});
                        let _ = send_json(sink, msg).await;
                    }
                }
                Err(e) => {
                    let msg = json!({"type": "error", "message": format!("并发闸门异常：{e}")});
                    let _ = send_json(sink, msg).await;
                }
            }
        }
        ClientMsg::Stop => {
            if let Err(e) = handle.stop() {
                tracing::warn!("[ws] stop 失败：{e:#}");
            }
        }
        ClientMsg::Ignore => {}
    }
}

async fn send_json(
    sink: &mut futures::stream::SplitSink<WebSocket, Message>,
    v: serde_json::Value,
) -> bool {
    sink.send(Message::Text(v.to_string().into())).await.is_ok()
}

enum ClientMsg {
    Chat(String),
    Stop,
    Ignore,
}

fn parse_client_msg(raw: &str) -> ClientMsg {
    #[derive(Deserialize)]
    #[serde(tag = "type")]
    enum Wire {
        #[serde(rename = "chat")]
        Chat { text: String },
        #[serde(rename = "stop")]
        Stop,
    }
    match serde_json::from_str::<Wire>(raw) {
        Ok(Wire::Chat { text }) if !text.trim().is_empty() => ClientMsg::Chat(text),
        Ok(Wire::Chat { .. }) => ClientMsg::Ignore,
        Ok(Wire::Stop) => ClientMsg::Stop,
        Err(_) => ClientMsg::Ignore,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, CostConfig, LlmConfig, SessionConfig, WebConfig};
    use crate::llm;
    use crate::web::AppState;
    use std::sync::Arc as StdArc;
    use tokio::time::Duration;

    /// max_concurrent=1：若并发许可在轮次终点没被回收，第二次 chat 会超时——
    /// 用最紧的闸门来验证「Error 事件 → 许可归还」这条链路。
    fn test_cfg(token: &str) -> Config {
        Config {
            llm: LlmConfig {
                // 端口 9（discard）几乎必然立即拒连：不联网、快速失败
                base_url: "http://127.0.0.1:9".into(),
                model: "test".into(),
                api_key_env: "CHEAPTRIP_TEST_KEY".into(),
                temperature: 0.7,
                max_tokens: 100,
                connect_timeout_secs: 1,
                read_timeout_secs: 1,
                provider: Default::default(),
                reasoning_effort: Default::default(),
            },
            cost: CostConfig {
                input_per_1m: 0.0,
                output_per_1m: 0.0,
            },
            session: SessionConfig {
                max_messages: None,
                max_sessions: None,
            },
            xhs: Default::default(),
            web: WebConfig {
                max_concurrent: 1,
                token: token.into(),
                ..Default::default()
            },
            auth: Default::default(),
        }
    }

    async fn spawn_server(token: &str) -> std::net::SocketAddr {
        let state: SharedState = StdArc::new(AppState::new(
            test_cfg(token),
            llm::Client::for_test(),
            StdArc::new(|_sid: &str| -> Vec<Box<crate::tools::DynTool>> { Vec::new() }),
        ));
        let app = super::super::router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        addr
    }

    async fn next_json(
        ws: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> serde_json::Value {
        loop {
            let msg = ws.next().await.unwrap().unwrap();
            if let tokio_tungstenite::tungstenite::Message::Text(t) = msg {
                return serde_json::from_str(&t).unwrap();
            }
        }
    }

    /// 等到指定变体的 AgentEvent（跳过 Step 等过程事件）。
    async fn wait_event(
        ws: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        key: &str,
    ) -> serde_json::Value {
        loop {
            let m = tokio::time::timeout(Duration::from_secs(10), next_json(ws))
                .await
                .unwrap();
            assert_eq!(m["type"], "event");
            if m["event"].as_object().is_some_and(|o| o.contains_key(key)) {
                return m;
            }
        }
    }

    /// 真链路：WS 握手 → 补历史 → chat → LLM 拒连产生 Error 事件 →
    /// 并发许可回收（max_concurrent=1 下第二轮 chat 仍能跑通）。
    #[tokio::test]
    async fn ws_roundtrip_error_and_permit_recycle() {
        let sid = "test-ws-roundtrip";
        let _ = Session::delete(sid);
        Session::new(sid.into()).save().unwrap();
        let addr = spawn_server("").await;
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws/{sid}"))
            .await
            .unwrap();

        // 首条是 history（含开场白）
        let history = tokio::time::timeout(Duration::from_secs(10), next_json(&mut ws))
            .await
            .unwrap();
        assert_eq!(history["type"], "history");
        assert_eq!(history["data"]["messages"].as_array().unwrap().len(), 1);

        // chat → LLM 拒连 → Error 事件
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            json!({"type": "chat", "text": "hi"}).to_string().into(),
        ))
        .await
        .unwrap();
        let err1 = wait_event(&mut ws, "Error").await;
        assert!(err1["event"]["Error"].as_str().is_some());

        // 第二轮仍能提交 = 并发许可已被 forwarder 回收
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            json!({"type": "chat", "text": "hi again"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
        let err2 = wait_event(&mut ws, "Error").await;
        assert!(err2["event"]["Error"].as_str().is_some());

        // stop 消息（此刻无轮在跑，应被静默接受不炸连接）
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            json!({"type": "stop"}).to_string().into(),
        ))
        .await
        .unwrap();

        let _ = ws.close(None).await;
        Session::delete(sid).unwrap();
    }

    /// WS 升级请求同样受口令保护：无 token 握手被 401 拒绝，带对 token 放行。
    #[tokio::test]
    async fn ws_auth_enforced() {
        let addr = spawn_server("sekrit").await;

        let denied = tokio_tungstenite::connect_async(format!("ws://{addr}/ws/test-ws-auth")).await;
        assert!(denied.is_err(), "无 token 的 WS 握手应被拒绝");

        let sid = "test-ws-auth";
        let _ = Session::delete(sid);
        Session::new(sid.into()).save().unwrap();
        let (mut ws, _) =
            tokio_tungstenite::connect_async(format!("ws://{addr}/ws/test-ws-auth?token=sekrit"))
                .await
                .unwrap();
        let history = tokio::time::timeout(Duration::from_secs(10), next_json(&mut ws))
            .await
            .unwrap();
        assert_eq!(history["type"], "history");
        let _ = ws.close(None).await;
        Session::delete("test-ws-auth").unwrap();
    }

    /// 客户端消息解析规则：chat 取文本、空文本忽略、非法 JSON 忽略、stop 直通。
    #[test]
    fn parse_client_msg_rules() {
        assert!(matches!(
            parse_client_msg(r#"{"type":"chat","text":" 你好 "}"#),
            ClientMsg::Chat(t) if t == " 你好 "
        ));
        assert!(matches!(
            parse_client_msg(r#"{"type":"chat","text":"   "}"#),
            ClientMsg::Ignore
        ));
        assert!(matches!(parse_client_msg("not json"), ClientMsg::Ignore));
        assert!(matches!(
            parse_client_msg(r#"{"type":"stop"}"#),
            ClientMsg::Stop
        ));
        assert!(matches!(
            parse_client_msg(r#"{"type":"unknown"}"#),
            ClientMsg::Ignore
        ));
    }
}

#[cfg(test)]
mod real_tests {
    use crate::session::Session;
    use tokio::time::Duration;

    /// 真 LLM 一轮（浏览器同款协议）：需要 .env 密钥与 config.toml。
    /// cargo test ws_real_llm_roundtrip -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn ws_real_llm_roundtrip() {
        dotenvy::dotenv().ok();
        let cfg = crate::config::Config::load().unwrap();
        let client = llm::Client::new(&cfg).unwrap();
        let sid = "test-ws-real";
        let _ = Session::delete(sid);
        Session::new(sid.into()).save().unwrap();
        let addr = spawn_server_with_tools(cfg, client).await;
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws/{sid}"))
            .await
            .unwrap();

        let history = tokio::time::timeout(Duration::from_secs(10), next_json(&mut ws))
            .await
            .unwrap();
        assert_eq!(history["type"], "history");

        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::json!({"type": "chat", "text": "只回复四个字：收到，出发"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();

        let mut content = String::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        loop {
            let m = tokio::time::timeout_at(deadline, next_json(&mut ws))
                .await
                .unwrap();
            if m["type"] != "event" {
                continue;
            }
            if let Some(c) = m["event"]["Content"].as_str() {
                content.push_str(c);
            }
            if let Some(d) = m["event"]["Done"].as_str() {
                println!("最终回复：{d}\n累计正文 {len} 字", len = content.len());
                assert!(!d.is_empty());
                break;
            }
            if let Some(e) = m["event"]["Error"].as_str() {
                panic!("意外错误：{e}");
            }
        }
        let _ = ws.close(None).await;
        Session::delete(sid).unwrap();
    }

    use super::super::AppState;
    use crate::llm;
    use crate::web::state::SharedState;
    use futures::{SinkExt as _, StreamExt as _};
    use std::sync::Arc as StdArc;

    async fn next_json(
        ws: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> serde_json::Value {
        loop {
            let msg = ws.next().await.unwrap().unwrap();
            if let tokio_tungstenite::tungstenite::Message::Text(t) = msg {
                return serde_json::from_str(&t).unwrap();
            }
        }
    }

    /// 带 client 的服务器（真实 Config/Client；工具列表留空，短回复用不到工具）。
    async fn spawn_server_with_tools(
        cfg: crate::config::Config,
        client: llm::Client,
    ) -> std::net::SocketAddr {
        let state: SharedState = StdArc::new(AppState::new(
            cfg,
            client,
            StdArc::new(|_sid: &str| -> Vec<Box<crate::tools::DynTool>> { Vec::new() }),
        ));
        let app = super::super::router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        addr
    }
}
