//! 车次查询工具（search_trains）：直连 12306 余票查询接口（免登录、实时准确），
//! 替代「search_web 搜车次」的幻觉/过时问题。
//!
//! 流程（低频查询，无登录）：GET `otn/leftTicket/init` 拿会话 cookie
//! → `queryG` 带 cookie+Referer 重放 → 解析 result 管道分隔行。
//! 站名→电报码映射来自官方 station_name.js（temp 目录缓存 7 天）。
//! 字段索引为 2026-09 实测：[3]车次 [4]出发站 [5]到达站 [8][9]时刻 [10]历时 [11]可购，
//! 席位：21高软/23软卧/26无座/28硬卧/29硬座/30二等/31一等/32商务。
//!
//! 纪律：单次调用 = 1 次查询，无重试轰炸；失败时提示 LLM 降级 search_web。

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::Tool;

const INIT_URL: &str = "https://kyfw.12306.cn/otn/leftTicket/init";
const QUERY_URL: &str = "https://kyfw.12306.cn/otn/leftTicket/queryG";
const STATION_JS_URL: &str = "https://kyfw.12306.cn/otn/resources/js/framework/station_name.js";
const UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/126.0.0.0 Safari/537.36";

/// 已实测验证的席位字段索引（值：有/无/数字/空=该席别无此车）。
const SEAT_FIELDS: &[(usize, &str)] = &[
    (32, "商务座"),
    (31, "一等座"),
    (30, "二等座"),
    (21, "高级软卧"),
    (23, "软卧"),
    (28, "硬卧"),
    (29, "硬座"),
    (26, "无座"),
];

pub struct SearchTrains {
    http: reqwest::Client,
}

impl SearchTrains {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .expect("构建 HTTP 客户端失败");
        Self { http }
    }
}

impl Default for SearchTrains {
    fn default() -> Self {
        Self::new()
    }
}

/// YYYY-MM-DD，缺省 = 今天+7（12306 起售窗口内，规划场景最常用）。
fn default_date() -> String {
    days_to_date(today_epoch_days() + 7)
}

/// epoch 天数 → (y, m, d)。Hinnant 算法，无需 chrono。
fn days_to_date(z: i64) -> String {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// YYYY-MM-DD → epoch 天数（civil_from_days，Hinnant 逆算法）。
fn date_to_days(s: &str) -> Option<i64> {
    if s.len() != 10 || s.as_bytes()[4] != b'-' || s.as_bytes()[7] != b'-' {
        return None;
    }
    let y: i64 = s[..4].parse().ok()?;
    let m: i64 = s[5..7].parse().ok()?;
    let d: i64 = s[8..10].parse().ok()?;
    // 月日合法性粗校验
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y_adj = if m <= 2 { y - 1 } else { y };
    let era = y_adj.div_euclid(400);
    let yoe = y_adj - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    (days_to_date(days) == s).then_some(days)
}

/// 今天（UTC+8）的 epoch 天数。
fn today_epoch_days() -> i64 {
    (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
        + 8 * 3600)
        .div_euclid(86400)
}

/// 12306 预售窗口保守估计：今天起 14 天内可售（窗口末 = today+13）。
const PRESALE_LAST_OFFSET: i64 = 13;
/// 超窗回退日期：today+10（远离窗口边缘的起售时间差，稳健）。
const FALLBACK_OFFSET: i64 = 10;

/// 查票候选日期（按用户策略）：客户日期已开售 → 只查当天；
/// 超预售窗 → 只查 today+10（稳健：远离窗口边缘的起售时间差，单候选不二连试）。
fn query_candidates(date: &str) -> Vec<String> {
    let today = today_epoch_days();
    match date_to_days(date) {
        Some(d) if d <= today + PRESALE_LAST_OFFSET => vec![date.to_string()],
        Some(_) => vec![days_to_date(today + FALLBACK_OFFSET)],
        None => vec![date.to_string()],
    }
}

/// 站名表：解析 station_name.js（`@编码|站名|电报码|拼音…`）→ (站名, 电报码) 列表。
fn parse_station_table(js: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for item in js.split('@') {
        let f: Vec<&str> = item.split('|').collect();
        if f.len() >= 3 && !f[1].is_empty() && !f[2].is_empty() {
            out.push((f[1].to_string(), f[2].to_string()));
        }
    }
    out
}

/// 站名解析：精确匹配 → 包含匹配（「长白山」→「长白山站」），返回电报码。
fn resolve_station<'a>(table: &'a [(String, String)], name: &str) -> Option<&'a str> {
    table
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, c)| c.as_str())
        .or_else(|| {
            table
                .iter()
                .find(|(n, _)| n.contains(name) || name.contains(n.as_str()))
                .map(|(_, c)| c.as_str())
        })
}

impl SearchTrains {
    /// station_name.js：temp 目录缓存 7 天。
    async fn station_table(&self) -> Result<Vec<(String, String)>> {
        let cache: PathBuf = std::env::temp_dir().join("cheaptrip_12306_station.js");
        let fresh = std::fs::metadata(&cache)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map(|e| e < Duration::from_secs(7 * 86400))
            .unwrap_or(false);
        let js = if fresh {
            std::fs::read_to_string(&cache).context("读取站名表缓存失败")?
        } else {
            let resp = self
                .http
                .get(STATION_JS_URL)
                .header("User-Agent", UA)
                .send()
                .await
                .context("下载 12306 站名表失败")?;
            let text = resp.error_for_status()?.text().await?;
            std::fs::write(&cache, &text).ok(); // 缓存失败不致命
            text
        };
        let table = parse_station_table(&js);
        if table.is_empty() {
            anyhow::bail!("站名表解析为空（12306 可能改版）");
        }
        Ok(table)
    }

    /// 一次查询：init 拿 cookie → queryG。
    async fn query(&self, date: &str, from: &str, to: &str) -> Result<Vec<String>> {
        let init = self
            .http
            .get(INIT_URL)
            .header("User-Agent", UA)
            .send()
            .await
            .context("访问 12306 init 失败")?;
        let cookies = init
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .filter_map(|c| c.split(';').next())
            .collect::<Vec<_>>()
            .join("; ");
        let url = format!(
            "{QUERY_URL}?leftTicketDTO.train_date={date}&leftTicketDTO.from_station={from}&leftTicketDTO.to_station={to}&purpose_codes=ADULT"
        );
        let resp = self
            .http
            .get(&url)
            .header("User-Agent", UA)
            .header("Referer", INIT_URL)
            .header("Cookie", &cookies)
            .send()
            .await
            .context("12306 余票查询失败")?;
        let final_url = resp.url().clone();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let text = resp.error_for_status()?.text().await?;
        if final_url.host_str() != Some("kyfw.12306.cn")
            || final_url.path().contains("/error")
            || !content_type.contains("json")
        {
            anyhow::bail!("12306 拒绝了该查询（日期可能无效或已过期）");
        }
        let v: Value = serde_json::from_str(&text).context("解析 12306 响应失败")?;
        Ok(v.pointer("/data/result")
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default())
    }
}

#[async_trait]
impl Tool for SearchTrains {
    fn name(&self) -> &str {
        "search_trains"
    }
    fn description(&self) -> &str {
        "查 12306 实时车次（出发/到达站名 + 日期）。返回车次、乘车站与到达站、时刻、历时、余票坐席。\
         日期超预售期（约 15 天）时自动改查最靠近的可售日并在结果中注明。\
         查火车/高铁/动车时刻与余票必须用此工具，不要用 search_web 搜车次（会过时/编造）。"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "from": {"type": "string", "description": "出发站名（如 北京 / 二道白河；带不带「站」字均可）"},
                "to": {"type": "string", "description": "到达站名"},
                "date": {"type": "string", "description": "出发日期 YYYY-MM-DD（可选，默认一周后；12306 起售窗口约 15 天）"}
            },
            "required": ["from", "to"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let from = args
            .get("from")
            .and_then(|v| v.as_str())
            .context("缺少 from 参数")?
            .trim()
            .trim_end_matches('站')
            .to_string();
        let to = args
            .get("to")
            .and_then(|v| v.as_str())
            .context("缺少 to 参数")?
            .trim()
            .trim_end_matches('站')
            .to_string();
        let date = args
            .get("date")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(default_date);
        let Some(date_days) = date_to_days(&date) else {
            return Ok(format!("日期格式应为 YYYY-MM-DD，收到「{date}」"));
        };
        if date_days < today_epoch_days() {
            return Ok(format!(
                "查询日期 {date} 已经过期；请根据当前日期 {} 提供今天或未来的出发日期。",
                days_to_date(today_epoch_days())
            ));
        }

        let table = self.station_table().await?;
        let from_code = resolve_station(&table, &from)
            .or_else(|| resolve_station(&table, &format!("{from}站")))
            .context("出发站名未识别")?;
        let to_code = resolve_station(&table, &to)
            .or_else(|| resolve_station(&table, &format!("{to}站")))
            .context("到达站名未识别")?;
        let rev: HashMap<&str, &str> = table
            .iter()
            .map(|(n, c)| (c.as_str(), n.as_str()))
            .collect();

        // 查票策略（query_candidates）：日期已开售 → 查当天；超预售窗 → 查 today+10（稳健）
        let candidates = query_candidates(&date);
        let used_date = candidates[0].clone();
        let mut note = String::new();
        if used_date != date {
            note = format!(
                "注意：{date} 尚未开售（12306 预售期约 15 天），已改查 {used_date} 的车次，仅供节奏/耗时参考。\n"
            );
        }
        let rows = self.query(&used_date, from_code, to_code).await?;
        if rows.is_empty() {
            return Ok(format!(
                "{date} {from}→{to} 无直达车次。可尝试：①换邻近大站（如地级市站）重新查 ②查中转方案（分段 search_trains）③search_web 补充参考。"
            ));
        }

        let mut lines = Vec::new();
        for row in &rows {
            let f: Vec<&str> = row.split('|').collect();
            if f.len() < 33 {
                continue;
            }
            let train = f[3];
            let dep_name = rev.get(f[4]).copied().unwrap_or(f[4]);
            let arr_name = rev.get(f[5]).copied().unwrap_or(f[5]);
            let (dep, arr, lishi) = (f[8], f[9], f[10]);
            let buyable = match f[11] {
                "Y" => "可购",
                "N" => "不可购",
                _ => f[1], // 未起售等提示（如「9月15日 8点起售」）
            };
            let seats: Vec<String> = SEAT_FIELDS
                .iter()
                .filter_map(|(i, name)| {
                    let v = f.get(*i)?.trim();
                    if v.is_empty() || v == "*" || v == "无" {
                        return None;
                    }
                    Some(format!("{name}:{v}"))
                })
                .collect();
            let seat_str = if seats.is_empty() {
                "余票信息暂无".to_string()
            } else {
                seats.join(" ")
            };
            lines.push(format!("{train} {dep_name} {dep} → {arr_name} {arr}（历时 {lishi}）| {buyable} | {seat_str}"));
        }
        if lines.is_empty() {
            return Ok(format!(
                "{date} {from}→{to} 查询结果解析为空（12306 可能改版），请用 search_web 交叉确认。"
            ));
        }
        let mut out = format!(
            "{note}{used_date} {from}({from_code})→{to}({to_code}) 共 {} 趟：\n",
            lines.len()
        );
        for l in lines {
            out.push_str(&format!("  {l}\n"));
        }
        out.push_str("提示：数据实时，余票随时变动；购买以 12306 为准。");
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 站名表解析 + 精确/模糊匹配。
    #[test]
    fn station_table_and_resolve() {
        let js = "var station_names='@bjb|北京|BJP|beijing|bjp|1@cbst|长白山站|CSLT|changbaishan|cbs|1@cc|长春|CCT|changchun|cc|2';";
        let table = parse_station_table(js);
        assert_eq!(table.len(), 3);
        assert_eq!(resolve_station(&table, "北京"), Some("BJP"));
        assert_eq!(resolve_station(&table, "长春"), Some("CCT"));
        // 模糊：「长白山」→「长白山站」
        assert_eq!(resolve_station(&table, "长白山"), Some("CSLT"));
        assert_eq!(resolve_station(&table, "不存在"), None);
    }

    /// 结果行解析：用实测 Z156 行构造（字段值取自 2026-09 真实响应）。
    #[test]
    fn parse_row_fields() {
        let mut f = vec![""; 58];
        f[1] = "预订";
        f[3] = "Z156";
        f[4] = "UTH";
        f[5] = "VAB";
        f[8] = "06:02";
        f[9] = "15:29";
        f[10] = "09:27";
        f[11] = "Y";
        f[23] = "1";
        f[26] = "无";
        f[28] = "13";
        f[29] = "无";
        let row = f.join("|");
        let fields: Vec<&str> = row.split('|').collect();
        assert_eq!(fields[3], "Z156");
        assert_eq!(fields[11], "Y");
        let seats: Vec<String> = SEAT_FIELDS
            .iter()
            .filter_map(|(i, name)| {
                let v = fields.get(*i)?.trim();
                if v.is_empty() || v == "*" || v == "无" {
                    return None;
                }
                Some(format!("{name}:{v}"))
            })
            .collect();
        assert_eq!(seats.join(" "), "软卧:1 硬卧:13");
    }

    /// 默认日期：civil 算法输出合法 YYYY-MM-DD。
    #[test]
    fn default_date_format() {
        let d = default_date();
        assert_eq!(d.len(), 10);
        assert_eq!(d.as_bytes()[4], b'-');
        let (y, m): (i64, u32) = (d[..4].parse().unwrap(), d[5..7].parse().unwrap());
        assert!((2026..=2100).contains(&y) && (1..=12).contains(&m));
    }

    /// 日期换算往返 + 查票候选策略：
    /// 窗口内日期只查当天；超窗（40 天后）→ 候选为窗口末与其前一天。
    #[test]
    fn date_roundtrip_and_candidates() {
        let today = today_epoch_days();
        for off in [0i64, 1, 7, 13, 40, 365] {
            let d = days_to_date(today + off);
            assert_eq!(date_to_days(&d), Some(today + off), "往返失败: {d}");
        }
        // 窗口内（明天）：单候选
        assert_eq!(
            query_candidates(&days_to_date(today + 1)),
            vec![days_to_date(today + 1)]
        );
        // 窗口末当天：单候选
        assert_eq!(
            query_candidates(&days_to_date(today + 13)),
            vec![days_to_date(today + 13)]
        );
        // 超窗（40 天后）：单候选 today+10
        let far = days_to_date(today + 40);
        assert_eq!(query_candidates(&far), vec![days_to_date(today + 10)]);
        // 非法日期原样透传（由查询层自然失败）
        assert_eq!(
            query_candidates("not-a-date"),
            vec!["not-a-date".to_string()]
        );
        assert_eq!(date_to_days("2026-02-29"), None);
        assert_eq!(date_to_days("2026-09-31"), None);
    }

    #[tokio::test]
    async fn past_date_is_rejected_before_network_access() {
        let tool = SearchTrains::new();
        let out = tool
            .execute(serde_json::json!({
                "from": "北京北",
                "to": "大同南",
                "date": "2025-10-04"
            }))
            .await
            .expect("过去日期应返回用户可读提示");
        assert!(out.contains("已经过期"), "输出异常: {out}");
    }

    /// 实跑：北京→长春（真实查询，几秒）。
    /// 运行：cargo test search_trains_live -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn search_trains_live() {
        let tool = SearchTrains::new();
        let out = tool
            .execute(serde_json::json!({"from": "北京", "to": "长春", "date": "2026-10-21"}))
            .await
            .expect("execute 失败");
        println!("\n=== search_trains 输出 ===\n{out}\n");
        assert!(out.contains("→"), "输出异常: {out}");
    }
}
