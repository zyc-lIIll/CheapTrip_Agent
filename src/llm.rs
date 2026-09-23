use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::pin::Pin;

use crate::config::Config;
use futures::{Stream, StreamExt};

pub mod provider;

// ---- 请求 ----

#[derive(Serialize, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// 流式时请求返回 usage（OpenAI 标准端点必须显式开启，否则流式无 token 统计）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

#[derive(Serialize, Clone)]
pub struct StreamOptions {
    pub include_usage: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Message {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    // GLM 推理模型可能返回的推理内容（OpenAI schema 之外的字段）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FunctionCall {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

#[derive(Serialize, Clone)]
pub struct Tool {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolFunction,
}

#[derive(Serialize, Clone)]
pub struct ToolFunction {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct Usage {
    /// 全部带 default：各厂商返回的 usage 字段不一，缺失不报错
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

// ---- 流式 ----

/// 流式增量事件。
pub enum StreamEvent {
    Content(String),
    Reasoning(String),
    ToolCallDelta {
        index: u32,
        id: Option<String>,
        name: Option<String>,
        args_chunk: Option<String>,
    },
    Usage(Usage),
    /// 流结束（finish_reason 可能为 stop/length/tool_calls）
    Finish {
        reason: Option<String>,
    },
    Done,
}

#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct Delta {
    #[serde(default)]
    #[allow(dead_code)]
    role: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Deserialize)]
struct ToolCallDelta {
    index: u32,
    #[serde(default)]
    id: Option<String>,
    function: ToolCallFnDelta,
}

#[derive(Deserialize)]
struct ToolCallFnDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

pub type StreamItem = Result<StreamEvent>;

// ---- 客户端 ----

pub struct Client {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    adapter: provider::RequestAdapter,
}

impl Clone for Client {
    fn clone(&self) -> Self {
        Self {
            http: self.http.clone(),
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
            adapter: self.adapter,
        }
    }
}

impl Client {
    pub fn new(cfg: &Config) -> Result<Self> {
        // 不设总超时：SSE 长流（长回答/长思考）必须允许长时间运行，总超时会中途掐断流。
        // connect_timeout 限建连；read_timeout 是空闲检测——连续无数据才计时，
        // 流式 reasoning 每个数据块到达都会重置（阈值可在 config.toml [llm] 配置，0=关闭）。
        let mut builder = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(cfg.llm.connect_timeout_secs));
        if cfg.llm.read_timeout_secs > 0 {
            builder =
                builder.read_timeout(std::time::Duration::from_secs(cfg.llm.read_timeout_secs));
        }
        let http = builder.build()?;
        Ok(Self {
            http,
            base_url: cfg.llm.base_url.clone(),
            api_key: cfg.api_key()?,
            adapter: provider::RequestAdapter::new(
                cfg.llm.provider,
                &cfg.llm.model,
                cfg.llm.reasoning_effort,
            ),
        })
    }

    /// 仅测试：不依赖环境变量的空客户端（Web 层 REST/WS 测试不会真正发起 LLM 请求）。
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        let http = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(std::time::Duration::from_secs(1))
            .read_timeout(std::time::Duration::from_secs(1))
            .build()
            .unwrap_or_default();
        Self {
            http,
            base_url: "http://127.0.0.1:9".into(),
            api_key: "test-key".into(),
            adapter: provider::RequestAdapter::new(
                provider::Provider::OpenaiCompatible,
                "test",
                provider::ReasoningEffort::Max,
            ),
        }
    }

    /// 流式聊天：返回一个增量事件流（SSE 手写解析）。
    pub async fn chat_stream(
        &self,
        mut req: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = StreamItem> + Send>>> {
        req.stream = Some(true);
        // OpenAI 标准端点流式默认不返回 usage，必须显式请求
        req.stream_options = Some(StreamOptions {
            include_usage: true,
        });
        self.adapter.adapt(&mut req);
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&req)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("LLM 流式请求失败 {status}: {body}");
        }
        let mut bytes = resp.bytes_stream();
        let stream = async_stream::stream! {
            let mut byte_buf: Vec<u8> = Vec::new();
            let mut buf = String::new();
            while let Some(chunk) = bytes.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        yield Err(anyhow::Error::from(e));
                        return;
                    }
                };
                // 网络分块按字节切，多字节 UTF-8 字符（中文 3 字节）可能跨 chunk。
                // 只取当前缓冲区里完整 UTF-8 的前缀，残留不完整字节留到下一轮。
                byte_buf.extend_from_slice(&chunk);
                let valid_up_to = match std::str::from_utf8(&byte_buf) {
                    Ok(s) => s.len(),
                    Err(e) => e.valid_up_to(),
                };
                if valid_up_to == 0 {
                    continue; // 整块都是不完整字符的前缀，等下个 chunk
                }
                let s = match String::from_utf8(byte_buf.drain(..valid_up_to).collect::<Vec<_>>()) {
                    Ok(s) => s,
                    Err(_) => continue, // 理论上不会发生（valid_up_to 已验证）
                };
                buf.push_str(&s);
                while let Some(idx) = buf.find("\n\n") {
                    let event = buf[..idx].to_string();
                    buf.drain(..idx + 2);
                    for line in event.lines() {
                        let data = match line.strip_prefix("data: ") {
                            Some(d) => d,
                            None => continue,
                        };
                        if data == "[DONE]" {
                            yield Ok(StreamEvent::Done);
                            return;
                        }
                        let chunk: StreamChunk = match serde_json::from_str(data) {
                            Ok(c) => c,
                            Err(e) => {
                                yield Err(anyhow::Error::from(e));
                                return;
                            }
                        };
                        for ch in chunk.choices {
                            if let Some(s) = ch.delta.content {
                                yield Ok(StreamEvent::Content(s));
                            }
                            if let Some(s) = ch.delta.reasoning_content {
                                yield Ok(StreamEvent::Reasoning(s));
                            }
                            if let Some(tcs) = ch.delta.tool_calls {
                                for tc in tcs {
                                    yield Ok(StreamEvent::ToolCallDelta {
                                        index: tc.index,
                                        id: tc.id,
                                        name: tc.function.name,
                                        args_chunk: tc.function.arguments,
                                    });
                                }
                            }
                            if let Some(fr) = ch.finish_reason {
                                yield Ok(StreamEvent::Finish { reason: Some(fr) });
                            }
                        }
                        if let Some(u) = chunk.usage {
                            yield Ok(StreamEvent::Usage(u));
                        }
                    }
                }
            }
        };
        Ok(Box::pin(stream))
    }
}
