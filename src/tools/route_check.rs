//! 两点驾车距离/时长查询工具（route_check）：封装高德驾车路径规划 API。
//!
//! 用途（打车口径的确定性数据）：
//! - 阶段2：散点到区核心 ≤90min 才算可当天往返的辐射景点；簇内最远两点 ≤30min 算「打车方便」
//! - 阶段3：行程耗时估算、远景点当天往返可行性
//!
//! 只收坐标（LLM 调用前先 geocode），保持工具单一职责；请求过共享 RateLimiter。

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use super::{RateLimiter, Tool};

pub struct RouteCheck {
    api_key: String,
    http: reqwest::Client,
    limiter: RateLimiter,
}

impl RouteCheck {
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

/// 端点：坐标必填，名称可选（仅用于结果展示）。
#[derive(Deserialize)]
struct Endpoint {
    #[serde(default)]
    name: Option<String>,
    lon: f64,
    lat: f64,
}

/// 工具入参。
#[derive(Deserialize)]
struct Args {
    origin: Endpoint,
    destination: Endpoint,
}

#[derive(Deserialize)]
struct DriveResp {
    status: String,
    #[serde(default)]
    info: String,
    #[serde(default)]
    route: Option<DriveRoute>,
}

#[derive(Deserialize)]
struct DriveRoute {
    #[serde(default)]
    paths: Vec<DrivePath>,
}

#[derive(Deserialize)]
struct DrivePath {
    #[serde(default)]
    distance: String, // 米
    #[serde(default)]
    duration: String, // 秒
}

impl Endpoint {
    fn label(&self) -> String {
        self.name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("({:.4},{:.4})", self.lon, self.lat))
    }
    /// 高德坐标参数格式「lon,lat」。
    fn coord(&self) -> String {
        format!("{:.6},{:.6}", self.lon, self.lat)
    }
}

#[async_trait]
impl Tool for RouteCheck {
    fn name(&self) -> &str {
        "route_check"
    }
    fn description(&self) -> &str {
        "查询两点间驾车距离与时长（≈打车口径）。输入必须是已 geocode 的经纬度坐标。\
         用途：抽查散点到交通便利区核心是否 ≤90 分钟（可当天往返）、\
         区内最远两点是否 ≤30 分钟（打车方便）、行程段耗时估算。"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "origin": {
                    "type": "object",
                    "description": "起点",
                    "properties": {
                        "name": {"type": "string", "description": "可选，名称（仅用于展示）"},
                        "lon": {"type": "number"},
                        "lat": {"type": "number"}
                    },
                    "required": ["lon", "lat"]
                },
                "destination": {
                    "type": "object",
                    "description": "终点",
                    "properties": {
                        "name": {"type": "string", "description": "可选，名称（仅用于展示）"},
                        "lon": {"type": "number"},
                        "lat": {"type": "number"}
                    },
                    "required": ["lon", "lat"]
                }
            },
            "required": ["origin", "destination"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String> {
        let args: Args = serde_json::from_value(args)
            .context("解析 route_check 参数失败（需 origin/destination）")?;

        for attempt in 0..2 {
            self.limiter.wait().await;
            let resp = self
                .http
                .get("https://restapi.amap.com/v3/direction/driving")
                .query(&[
                    ("key", self.api_key.as_str()),
                    ("origin", args.origin.coord().as_str()),
                    ("destination", args.destination.coord().as_str()),
                    ("extensions", "base"),
                    ("output", "JSON"),
                ])
                .send()
                .await
                .context("高德驾车路径请求失败")?;
            let text = resp.text().await.context("读取响应体失败")?;
            let body: DriveResp =
                serde_json::from_str(&text).with_context(|| format!("解析响应失败: {text}"))?;
            if body.status == "1" {
                let path = body
                    .route
                    .and_then(|r| r.paths.into_iter().next())
                    .ok_or_else(|| anyhow::anyhow!("高德返回无可用驾车路径"))?;
                // 解析失败说明响应结构异常，如实报错而不是伪装成 0 km
                let (dist_m, dur_s) = match (
                    path.distance.trim().parse::<f64>(),
                    path.duration.trim().parse::<f64>(),
                ) {
                    (Ok(d), Ok(s)) => (d, s),
                    _ => {
                        return Ok(format!(
                            "驾车路径查询失败：响应数据异常（distance={:?}, duration={:?}）",
                            path.distance, path.duration
                        ));
                    }
                };
                return Ok(format!(
                    "{} → {}：驾车约 {:.1} km，约 {:.0} 分钟（打车口径）",
                    args.origin.label(),
                    args.destination.label(),
                    dist_m / 1000.0,
                    dur_s / 60.0
                ));
            }
            // QPS 限流：等 1.2s 重试一次；其他错误直接报
            if body.info.contains("QPS") && attempt == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
                continue;
            }
            return Ok(format!("驾车路径查询失败：{}", body.info));
        }
        Ok("驾车路径查询失败：重试次数用尽".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 实跑：延安市区 → 壶口瀑布（约 100+ km，验证距离/时长解析）。
    /// 运行：cargo test route_check_real -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn route_check_real() {
        let _ = dotenvy::dotenv();
        let key = std::env::var("AMAP_API_KEY").expect("需设置 AMAP_API_KEY");
        let tool = RouteCheck::new(key, RateLimiter::new(400));
        let out = tool
            .execute(serde_json::json!({
                "origin": {"name": "延安市区", "lon": 109.49, "lat": 36.60},
                "destination": {"name": "壶口瀑布", "lon": 110.47, "lat": 36.13}
            }))
            .await
            .expect("execute 失败");
        println!("\n=== route_check 输出 ===\n{out}\n");
        assert!(out.contains("驾车约"), "输出异常: {out}");
    }
}
