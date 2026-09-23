//! search_web 工具：通过 Exa API 搜索网页，返回结果摘要给 LLM。
//! Exa 文档：https://docs.exa.ai/reference/search-api-guide-for-coding-agents

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Tool;

pub struct SearchWeb {
    api_key: String,
    http: reqwest::Client,
}

impl SearchWeb {
    pub fn new(api_key: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("构建 HTTP 客户端失败");
        Self { api_key, http }
    }
}

// ---- Exa API 请求/响应类型 ----

#[derive(Serialize)]
struct ExaRequest {
    query: String,
    #[serde(rename = "type")]
    kind: String,
    num_results: usize,
    contents: ExaContents,
}

#[derive(Serialize)]
struct ExaContents {
    highlights: bool,
}

#[derive(Deserialize)]
struct ExaResponse {
    results: Vec<ExaResult>,
}

#[derive(Deserialize)]
struct ExaResult {
    title: Option<String>,
    url: Option<String>,
    highlights: Option<Vec<String>>,
}

#[async_trait]
impl Tool for SearchWeb {
    fn name(&self) -> &str {
        "search_web"
    }
    fn description(&self) -> &str {
        "搜索网页信息。用于查景点、美食、车次、酒店、门票价格、开放时间等。返回搜索结果摘要。"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "搜索关键词" }
            },
            "required": ["query"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .context("search_web 缺少 query 参数")?;

        let req = ExaRequest {
            query: query.to_string(),
            kind: "auto".into(),
            num_results: 5,
            contents: ExaContents { highlights: true },
        };

        let resp = self
            .http
            .post("https://api.exa.ai/search")
            .header("x-api-key", &self.api_key)
            .json(&req)
            .send()
            .await
            .context("Exa 搜索请求失败")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Ok(format!("搜索失败（{status}）: {body}"));
        }

        let exa: ExaResponse = resp.json().await.context("解析 Exa 响应失败")?;

        if exa.results.is_empty() {
            return Ok(format!("搜索「{query}」未找到结果。"));
        }

        // 拼成 LLM 易读的文本摘要
        let mut out = format!("搜索「{query}」找到 {} 条结果：\n\n", exa.results.len());
        for (i, r) in exa.results.iter().enumerate() {
            out.push_str(&format!(
                "{}. {}\n",
                i + 1,
                r.title.as_deref().unwrap_or("无标题")
            ));
            if let Some(url) = &r.url {
                out.push_str(&format!("   链接: {url}\n"));
            }
            if let Some(hs) = &r.highlights {
                for h in hs {
                    out.push_str(&format!("   {h}\n"));
                }
            }
            out.push('\n');
        }
        Ok(out)
    }
}
