//! 酒店评论爬取工具（search_hotel_reviews）：调用本机 Node Playwright 脚本
//! （scripts/hotel_crawl/crawl.mjs）抓取携程酒店评论，产出结构化摘要供 LLM 总结，
//! 实拍图片落盘 hotels/{session_id}/hotel_{n}/images/。
//!
//! 环境前置（缺任一即返回友好错误并建议降级 search_web，不阻塞对话）：
//! - node ≥ 20 + scripts/hotel_crawl/ 下 `npm i playwright` + chromium
//! - 已运行 `./trip hotel login` 保存登录态 .ctrip-state.json（携程评论需登录可见）
//!
//! 纪律：脚本全程只读、慢速滚动；单次调用约 1~3 分钟（限流性等待是刻意的拟人节奏）。

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use super::Tool;

pub struct HotelReviews {
    session_id: String,
    script_dir: PathBuf,
    counter: AtomicU32,
}

impl HotelReviews {
    pub fn new(session_id: String) -> Self {
        let dir = Path::new("hotels").join(sanitize(&session_id));
        let mut max = 0u32;
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if let Some(n) = name
                    .strip_prefix("hotel_")
                    .and_then(|s| s.split('_').next())
                    .and_then(|s| s.parse::<u32>().ok())
                {
                    max = max.max(n);
                }
            }
        }
        Self {
            session_id,
            script_dir: PathBuf::from("scripts/hotel_crawl"),
            counter: AtomicU32::new(max),
        }
    }

    /// 环境自检：脚本/登录态/依赖三样齐全才可运行（main.rs 注册前也用此判断）。
    pub fn env_ready(&self) -> std::result::Result<(), String> {
        if !self.script_dir.join("crawl.mjs").exists() {
            return Err("爬虫脚本缺失（scripts/hotel_crawl/crawl.mjs）".into());
        }
        if !self.script_dir.join(".ctrip-state.json").exists() {
            return Err("携程登录态缺失（先运行 ./trip hotel login 扫码）".into());
        }
        if !self.script_dir.join("node_modules").exists() {
            return Err("playwright 未安装（先在 scripts/hotel_crawl 运行 npm i playwright && npx playwright install chromium）".into());
        }
        Ok(())
    }
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// 提取携程酒店 id：支持完整 URL（hotels.ctrip.com/hotels/1286148.html、?hotelId=1286148）或纯数字 id。
fn parse_hotel_id(s: &str) -> Option<String> {
    let digits_after = |marker: &str| {
        s.find(marker).and_then(|i| {
            let rest = &s[i + marker.len()..];
            let d: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if d.len() >= 4 { Some(d) } else { None }
        })
    };
    if s.contains("ctrip.com") || s.contains("trip.com") {
        digits_after("hotelId=")
            .or_else(|| digits_after("hotels/"))
            .or_else(|| digits_after("hoteldetail/"))
    } else {
        let t = s.trim();
        if !t.is_empty() && t.len() >= 4 && t.chars().all(|c| c.is_ascii_digit()) {
            Some(t.to_string())
        } else {
            None
        }
    }
}

#[async_trait]
impl Tool for HotelReviews {
    fn name(&self) -> &str {
        "search_hotel_reviews"
    }
    fn description(&self) -> &str {
        "爬取携程酒店的真实评论（评分/子评分/差评数/好评关键词/差评与好评采样/住客实拍图）。\
         输入携程酒店 URL（search_web 用 site:hotels.ctrip.com 搜「{城市} {酒店名}」获取）或纯数字酒店 id。\
         运行较慢（1~3 分钟，内置拟人限流），每次调用爬一家酒店。"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "hotel_url_or_id": {"type": "string", "description": "携程酒店详情页 URL 或纯数字酒店 id"},
                "dislike_limit": {"type": "integer", "description": "可选，差评采样上限（默认 15，带图/长文优先）"},
                "good_limit": {"type": "integer", "description": "可选，好评采样上限（默认 10，按字数取最长）"},
                "image_limit": {"type": "integer", "description": "可选，实拍图下载上限（默认与上限均为 5，拟人纪律）"},
                "checkin": {"type": "string", "description": "可选，入住日期 YYYY-MM-DD（默认一周后）；价格按此日期查"},
                "checkout": {"type": "string", "description": "可选，退房日期 YYYY-MM-DD（默认入住次日）"}
            },
            "required": ["hotel_url_or_id"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let raw = args
            .get("hotel_url_or_id")
            .and_then(|v| v.as_str())
            .context("缺少 hotel_url_or_id 参数")?;
        let Some(hotel_id) = parse_hotel_id(raw) else {
            return Ok(format!(
                "酒店评论爬取失败：无法从「{raw}」解析出携程酒店 id。\
                 请先用 search_web 搜「site:hotels.ctrip.com {raw}」拿详情页 URL（形如 hotels.ctrip.com/hotels/1286148.html）再重试。"
            ));
        };
        if let Err(e) = self.env_ready() {
            return Ok(format!(
                "酒店评论爬取不可用：{e}。本次改用 search_web 查该酒店的口碑信息即可。"
            ));
        }

        let n = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
        let out_dir = Path::new("hotels")
            .join(sanitize(&self.session_id))
            .join(format!("hotel_{n}"));
        std::fs::create_dir_all(&out_dir).context("创建 hotels 输出目录失败")?;

        let dislike = args
            .get("dislike_limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(15);
        let good = args
            .get("good_limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(10);
        let images = args
            .get("image_limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(5)
            .min(5);

        let mut cmd = tokio::process::Command::new("node");
        cmd.current_dir(&self.script_dir)
            .arg("crawl.mjs")
            .arg(&hotel_id)
            .arg("--out")
            .arg(&out_dir)
            .arg("--dislike")
            .arg(dislike.to_string())
            .arg("--good")
            .arg(good.to_string())
            .arg("--images")
            .arg(images.to_string());
        if let Some(d) = args.get("checkin").and_then(|v| v.as_str()) {
            cmd.arg("--checkin").arg(d);
        }
        if let Some(d) = args.get("checkout").and_then(|v| v.as_str()) {
            cmd.arg("--checkout").arg(d);
        }
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true); // 调用被取消/超时时杀掉浏览器子进程

        let child = cmd
            .spawn()
            .context("启动爬虫脚本失败（检查 node 是否可用）")?;
        let run = tokio::time::timeout(
            std::time::Duration::from_secs(300),
            child.wait_with_output(),
        )
        .await;
        let output = match run {
            Ok(r) => r.context("等待爬虫脚本失败")?,
            Err(_) => {
                return Ok(format!(
                    "酒店评论爬取超时（300s）：{hotel_id}。可能是酒店评论量过大，建议稍后重试一次，或改用 search_web 查口碑。"
                ));
            }
        };
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        // exit 2 = 爬虫明确报告登录态失效：给出重新登录指引
        if output.status.code() == Some(2) {
            return Ok(
                "携程登录态已失效，评论爬取中止。请运行 `./trip hotel login` 重新扫码后再试；或改用 search_web 查该酒店口碑，不阻塞当前工作。"
                    .to_string(),
            );
        }
        if !output.status.success() {
            let tail: String = stderr
                .lines()
                .rev()
                .take(6)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n");
            return Ok(format!(
                "酒店评论爬取失败（exit {}）：{}\n可改用 search_web 查该酒店口碑，不阻塞当前工作。",
                output.status, tail
            ));
        }
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let v: Value = serde_json::from_str(stdout.trim())
            .with_context(|| format!("解析爬虫输出失败: {}", &stdout[..stdout.len().min(300)]))?;

        if v.get("name").and_then(|x| x.as_str()).is_none() {
            return Ok(format!(
                "酒店评论爬取失败：{hotel_id} 页面未解析出酒店信息（可能 id 有误或页面改版）。可改用 search_web 查口碑。"
            ));
        }
        Ok(render_summary(&v, &out_dir.display().to_string()))
    }
}

/// 把爬虫 JSON 渲染成 LLM 易读的紧凑摘要。
fn render_summary(v: &Value, image_dir: &str) -> String {
    let get = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("未知");
    let num = |k: &str| -> String {
        match v.get(k) {
            Some(Value::Number(n)) => n.to_string(),
            Some(Value::String(s)) => s.clone(),
            _ => "未知".into(),
        }
    };
    let mut out = format!(
        "已爬取酒店评论：{}（评分 {}，评论 {} 条，{}）\n",
        get("name"),
        num("rating"),
        num("reviewCount"),
        get("opened")
    );
    if let Some(rooms) = v
        .get("rooms")
        .and_then(|x| x.as_array())
        .filter(|r| !r.is_empty())
    {
        out.push_str("房型实时价（起价/晚）：\n");
        for r in rooms {
            out.push_str(&format!(
                "  {} — ¥{}\n",
                r.get("name").and_then(|x| x.as_str()).unwrap_or("?"),
                r.get("price").map(|p| p.to_string()).unwrap_or("?".into())
            ));
        }
    }
    let sub = v.get("subRatings").and_then(|x| x.as_object());
    if let Some(m) = sub {
        let parts: Vec<String> = m
            .iter()
            .map(|(k, val)| format!("{k} {}", val.as_f64().unwrap_or(0.0)))
            .collect();
        out.push_str(&format!("子评分：{} | ", parts.join(" ")));
    }
    if let Some(d) = v.get("dislikeCount").and_then(|x| x.as_u64()) {
        out.push_str(&format!("差评 {d} 条\n"));
    }
    let tags = v
        .get("keywordTags")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|t| match (t.get("word"), t.get("count")) {
                    (Some(w), Some(c)) => Some(format!(
                        "{}({})",
                        w.as_str().unwrap_or(""),
                        c.as_u64().unwrap_or(0)
                    )),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    if !tags.is_empty() {
        out.push_str(&format!("好评关键词：{tags}\n"));
    }

    let render_list = |key: &str, title: &str, out: &mut String| {
        if let Some(list) = v.get(key).and_then(|x| x.as_array()) {
            out.push_str(&format!("\n{title}（{} 条采样）：\n", list.len()));
            for (i, c) in list.iter().enumerate() {
                let score = c.get("score").map(|s| s.to_string()).unwrap_or("?".into());
                let date = c.get("date").and_then(|x| x.as_str()).unwrap_or("");
                let prov = c.get("province").and_then(|x| x.as_str()).unwrap_or("");
                let text = c
                    .get("text")
                    .and_then(|x| x.as_str())
                    .unwrap_or("（未取到原文）");
                let reply = c
                    .get("hotelReply")
                    .and_then(|x| x.as_str())
                    .map(|r| format!("〔酒店回复：{}〕", r.chars().take(80).collect::<String>()))
                    .unwrap_or_default();
                let imgs = c
                    .get("images")
                    .map(|x| x.as_array().map_or(0, |a| a.len()))
                    .unwrap_or(0);
                let brief: String = text.chars().take(220).collect();
                out.push_str(&format!(
                    "{}. [{}分 {} {}] {}{}{}\n",
                    i + 1,
                    score,
                    date,
                    prov,
                    brief,
                    if imgs > 0 {
                        format!("（含图{imgs}）")
                    } else {
                        String::new()
                    },
                    reply
                ));
            }
        }
    };
    render_list("dislikes", "差评采样（带图/长文优先，避坑重点）", &mut out);
    render_list("good", "好评采样（长文优先）", &mut out);

    let img_count = v
        .get("images")
        .and_then(|x| x.as_array())
        .map_or(0, |a| a.len());
    out.push_str(&format!(
        "\n住客实拍图 {img_count} 张已存 {image_dir}/images/（Web 端可内联展示）。\n\
         使用提示：差评与酒店回复是避坑金矿，总结时提炼共性问题；评分仅作参考，以差评为准排查硬伤。"
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 酒店 id 解析：URL 两种格式 + 纯数字。
    #[test]
    fn parse_hotel_id_forms() {
        assert_eq!(
            parse_hotel_id("https://hotels.ctrip.com/hotels/1286148.html").unwrap(),
            "1286148"
        );
        assert_eq!(
            parse_hotel_id("https://hotels.ctrip.com/hotels/detail/?hotelId=1286148").unwrap(),
            "1286148"
        );
        assert_eq!(
            parse_hotel_id("https://m.ctrip.com/webapp/hotel/hoteldetail/1286148.html").unwrap(),
            "1286148"
        );
        assert_eq!(parse_hotel_id("1286148").unwrap(), "1286148");
        assert!(parse_hotel_id("汉庭酒店").is_none());
    }

    /// 摘要渲染：字段缺失不 panic。
    #[test]
    fn render_summary_tolerates_missing_fields() {
        let v: Value = serde_json::json!({
            "name": "测试酒店", "rating": 4.6, "reviewCount": "1.3万", "opened": "2021年开业",
            "subRatings": {"卫生": 4.7}, "dislikeCount": 12,
            "keywordTags": [{"word": "位置好", "count": 100}],
            "dislikes": [{"score": 2.0, "date": "2026年08月", "province": "北京", "text": "空调异响", "hotelReply": "酒店回复: 抱歉", "images": ["a","b"]}],
            "good": [], "images": ["x.jpg"]
        });
        let s = render_summary(&v, "hotels/test/hotel_1");
        assert!(s.contains("测试酒店"));
        assert!(s.contains("差评 12 条"));
        assert!(s.contains("位置好(100)"));
        assert!(s.contains("空调异响"));
        assert!(s.contains("含图2"));
        assert!(s.contains("〔酒店回复"));
        let empty: Value = serde_json::json!({});
        assert!(!render_summary(&empty, "x").is_empty());
    }

    /// 实跑：爬北京索菲特大酒店（1.3 万条评论的大家伙，约 2~3 分钟）。
    /// 前置：`./trip hotel login` 已保存登录态、playwright 已安装。
    /// 运行：cargo test hotel_reviews_live -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn hotel_reviews_live() {
        let tool = HotelReviews::new("test".into());
        tool.env_ready()
            .expect("环境未就绪：先在 scripts/hotel_crawl 跑 login.mjs 与 npm i");
        let out = tool
            .execute(serde_json::json!({"hotel_url_or_id": "1286148", "dislike_limit": 5, "good_limit": 3}))
            .await
            .expect("execute 失败");
        println!("\n=== search_hotel_reviews 输出 ===\n{out}\n");
        assert!(out.contains("已爬取酒店评论"), "输出异常: {out}");
    }
}
