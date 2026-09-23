//! 天气查询工具：通过高德地图 API 查询实时天气与多日预报，供旅游出行建议参考。
//! 高德天气 API 要求 city 传 adcode（区域编码），先用地理编码 API 把城市名解析成 adcode，
//! 再顺序拉取实况（base）+ 预报（all，未来4天），请求经共享 RateLimiter 自动间隔。
//! 文档：https://lbs.amap.com/api/webservice/guide/api-advanced/weatherinfo
//! AMAP_API_KEY 同时用于地图工具（阶段7c）。

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use super::{RateLimiter, Tool};

pub struct GetWeather {
    api_key: String,
    http: reqwest::Client,
    limiter: RateLimiter,
}

impl GetWeather {
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

    /// 城市名 → adcode（高德天气接口要求传 adcode）。
    /// 失败时返回 Err，由调用方决定如何提示用户。
    async fn resolve_adcode(&self, city: &str) -> Result<(String, String)> {
        self.limiter.wait().await;
        let resp = self
            .http
            .get("https://restapi.amap.com/v3/geocode/geo")
            .query(&[
                ("key", self.api_key.as_str()),
                ("address", city),
                ("output", "JSON"),
            ])
            .send()
            .await
            .context("高德地理编码请求失败")?;
        let body: GeoResp = resp.json().await.context("解析高德地理编码响应失败")?;
        if body.status != "1" {
            anyhow::bail!(
                "高德地理编码返回错误: status={} info={}",
                body.status,
                body.info
            );
        }
        let g = body
            .geocodes
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("无法解析城市「{city}」的编码"))?;
        let display = if !g.city.is_empty() {
            g.city
        } else if !g.province.is_empty() {
            g.province
        } else {
            city.to_string()
        };
        Ok((g.adcode, display))
    }

    /// 拉取一次天气（extensions: "base" 实况 / "all" 预报）。
    async fn fetch(&self, adcode: &str, extensions: &str) -> Result<WeatherResp> {
        self.limiter.wait().await;
        let resp = self
            .http
            .get("https://restapi.amap.com/v3/weather/weatherInfo")
            .query(&[
                ("key", self.api_key.as_str()),
                ("city", adcode),
                ("extensions", extensions),
                ("output", "JSON"),
            ])
            .send()
            .await
            .context("高德天气请求失败")?;
        let body: WeatherResp = resp.json().await.context("解析高德天气响应失败")?;
        if body.status != "1" {
            anyhow::bail!(
                "高德天气接口返回错误: status={} info={}",
                body.status,
                body.info
            );
        }
        Ok(body)
    }
}

// ---- 地理编码响应 ----

#[derive(Deserialize)]
struct GeoResp {
    status: String,
    #[serde(default)]
    info: String,
    #[serde(default)]
    geocodes: Vec<GeoCode>,
}

#[derive(Deserialize)]
struct GeoCode {
    #[serde(default)]
    adcode: String,
    #[serde(default)]
    province: String,
    #[serde(default)]
    city: String,
}

// ---- 天气响应 ----

#[derive(Deserialize)]
struct WeatherResp {
    status: String,
    #[serde(default)]
    info: String,
    #[serde(default)]
    lives: Vec<Live>,
    #[serde(default)]
    forecasts: Vec<Forecast>,
}

#[derive(Deserialize)]
struct Live {
    #[serde(default)]
    weather: String,
    #[serde(default)]
    temperature: String,
    #[serde(default)]
    winddirection: String,
    #[serde(default)]
    windpower: String,
    #[serde(default)]
    humidity: String,
    #[serde(default)]
    reporttime: String,
}

#[derive(Deserialize)]
struct Forecast {
    #[serde(default)]
    reporttime: String,
    #[serde(default)]
    casts: Vec<Cast>,
}

#[derive(Deserialize)]
struct Cast {
    #[serde(default)]
    date: String,
    #[serde(default)]
    week: String,
    #[serde(default)]
    dayweather: String,
    #[serde(default)]
    nightweather: String,
    #[serde(default)]
    daytemp: String,
    #[serde(default)]
    nighttemp: String,
    #[serde(default)]
    daywind: String,
    #[serde(default)]
    nightwind: String,
    #[serde(default)]
    daypower: String,
    #[serde(default)]
    nightpower: String,
}

#[async_trait]
impl Tool for GetWeather {
    fn name(&self) -> &str {
        "get_weather"
    }
    fn description(&self) -> &str {
        "查询指定城市的天气（实时天气 + 未来4天预报）。用于旅游出行建议、衣物准备、是否带伞等。城市名传中文名称即可，如「北京」「杭州」。"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "location": {
                    "type": "string",
                    "description": "城市中文名称，如「北京」「上海」「杭州」"
                }
            },
            "required": ["location"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String> {
        let location = args
            .get("location")
            .and_then(|v| v.as_str())
            .context("get_weather 缺少 location 参数")?;

        // 1. 城市名 → adcode
        let (adcode, display) = match self.resolve_adcode(location).await {
            Ok(v) => v,
            Err(e) => return Ok(format!("查询「{location}」天气失败：{e}")),
        };

        // 2. 顺序拉取实况 + 预报（fetch 内部经 RateLimiter 自动间隔，不并发）
        let base = self.fetch(&adcode, "base").await;
        let all = self.fetch(&adcode, "all").await;

        let mut out = String::new();

        // 实况
        if let Ok(b) = &base
            && let Some(live) = b.lives.first()
        {
            out.push_str(&format!(
                "{display} 实况天气（{time}）：\n  天气：{w}  气温：{t}℃  湿度：{h}%\n  风向：{wd}  风力：{wp}级\n\n",
                time = live.reporttime,
                w = live.weather,
                t = live.temperature,
                h = live.humidity,
                wd = live.winddirection,
                wp = live.windpower,
            ));
        }

        // 预报
        if let Ok(a) = &all
            && let Some(fc) = a.forecasts.first()
        {
            out.push_str(&format!(
                "未来预报（发布于 {time}）：\n",
                time = fc.reporttime
            ));
            for c in &fc.casts {
                out.push_str(&format!(
                    "  {date} 周{wk}：白天 {dw} {dt}℃ / 夜间 {nw} {nt}℃；风 {dwn} {dp}级 / {nwn} {np}级\n",
                    date = c.date,
                    wk = week_cn(&c.week),
                    dw = c.dayweather,
                    dt = c.daytemp,
                    nw = c.nightweather,
                    nt = c.nighttemp,
                    dwn = c.daywind,
                    dp = c.daypower,
                    nwn = c.nightwind,
                    np = c.nightpower,
                ));
            }
        }

        if out.is_empty() {
            // 都失败或无数据
            if let Err(e) = &base {
                return Ok(format!("查询「{location}」天气失败：{e}"));
            }
            if let Err(e) = &all {
                return Ok(format!("查询「{location}」预报失败：{e}"));
            }
            return Ok(format!("未查到「{location}」的天气数据。"));
        }

        Ok(out.trim_end().to_string())
    }
}

/// 高德 week 字段（"1".."7"，1=周一）→ 中文。
fn week_cn(s: &str) -> &str {
    match s {
        "1" => "一",
        "2" => "二",
        "3" => "三",
        "4" => "四",
        "5" => "五",
        "6" => "六",
        "7" => "日",
        _ => "?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实联网测试：需 AMAP_API_KEY。默认被 ignore，
    /// 运行：`cargo test weather_beijing_real -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn weather_beijing_real() {
        let _ = dotenvy::dotenv();
        let key = std::env::var("AMAP_API_KEY").expect("需设置 AMAP_API_KEY");
        let tool = GetWeather::new(key, RateLimiter::new(400));
        let out = tool
            .execute(serde_json::json!({ "location": "北京" }))
            .await
            .expect("execute 失败");
        println!("\n=== get_weather 输出 ===\n{out}\n");
        assert!(
            out.contains("实况天气") || out.contains("预报"),
            "输出不像天气数据: {out}"
        );
    }
}
