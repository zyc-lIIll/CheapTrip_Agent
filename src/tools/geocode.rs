//! 地理编码工具：调高德 geocode API，输入地名返回经纬度。
//! 地图工具的前置依赖——LLM 生成任何地图前先调此工具拿坐标。

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use super::{RateLimiter, Tool};

pub struct Geocode {
    api_key: String,
    http: reqwest::Client,
    limiter: RateLimiter,
}

impl Geocode {
    pub fn new(api_key: String, limiter: RateLimiter) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("构建 HTTP 客户端失败");
        Self {
            api_key,
            http,
            limiter,
        }
    }
}

#[derive(Deserialize)]
struct GeoResp {
    status: String,
    #[serde(default)]
    info: String,
    #[serde(default)]
    geocodes: Vec<GeoCode>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct GeoCode {
    #[serde(default)]
    location: String,
    #[serde(default)]
    province: serde_json::Value,
    #[serde(default)]
    city: serde_json::Value,
    #[serde(default)]
    district: serde_json::Value,
    #[serde(default)]
    formatted_address: String,
}

#[async_trait]
impl Tool for Geocode {
    fn name(&self) -> &str {
        "geocode"
    }
    fn description(&self) -> &str {
        "地理编码：输入地名（城市/景点/地址），返回经纬度。生成地图前必须先调此工具拿坐标。"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "address": {
                    "type": "string",
                    "description": "地名，如「北京」「故宫」「杭州西湖」"
                },
                "city": {
                    "type": "string",
                    "description": "可选，指定城市范围避免歧义，如「北京」"
                }
            },
            "required": ["address"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String> {
        let address = args
            .get("address")
            .and_then(|v| v.as_str())
            .context("geocode 缺少 address 参数")?;
        let city = args.get("city").and_then(|v| v.as_str());

        let mut params: Vec<(&str, &str)> = vec![
            ("key", self.api_key.as_str()),
            ("address", address),
            ("output", "JSON"),
        ];
        if let Some(c) = city {
            params.push(("city", c));
        }

        // LLM 一轮里可能连调多次 geocode，限流器保证相邻请求 ≥400ms 不触发 QPS
        self.limiter.wait().await;
        let resp = self
            .http
            .get("https://restapi.amap.com/v3/geocode/geo")
            .query(&params)
            .send()
            .await
            .context("高德地理编码请求失败")?;
        let text = resp.text().await.context("读取响应体失败")?;
        let body: GeoResp =
            serde_json::from_str(&text).with_context(|| format!("解析失败: {text}"))?;

        if body.status != "1" {
            return Ok(format!("地理编码失败：{info}", info = body.info));
        }

        let g = match body.geocodes.into_iter().next() {
            Some(g) => g,
            None => return Ok(format!("未找到「{address}」的坐标")),
        };

        let (lon, lat) = {
            let mut it = g.location.split(',');
            let lon = it.next().unwrap_or("0");
            let lat = it.next().unwrap_or("0");
            (lon, lat)
        };

        Ok(format!(
            "「{address}」→ 经度 {lon}，纬度 {lat}（{formatted}）",
            formatted = g.formatted_address
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 实跑：geocode 查北京。运行：cargo test geocode_real -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn geocode_real() {
        let _ = dotenvy::dotenv();
        let key = std::env::var("AMAP_API_KEY").expect("需设置 AMAP_API_KEY");
        let tool = Geocode::new(key, RateLimiter::new(400));
        let out = tool
            .execute(serde_json::json!({"address": "北京"}))
            .await
            .expect("execute 失败");
        println!("\n=== geocode 输出 ===\n{out}\n");
        assert!(out.contains("116"), "输出不像坐标: {out}");
    }
}
