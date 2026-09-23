//! 工具系统：每个工具一个文件，在此 pub use 统一导出。
//! 新增工具：① 在 `tools/` 下建文件实现 `Tool` trait ② 在此 `pub mod` + `pub use` ③ 在 main.rs 注册。

pub mod cluster_pois;
pub mod geocode;
pub mod hotel_reviews;
pub mod knowledge;
pub mod map;
pub mod mode;
pub mod phase;
pub mod route_check;
pub mod search_web;
pub mod trains;
pub mod update_notes;
pub mod weather;
pub mod xhs;

pub use cluster_pois::ClusterPois;
pub use geocode::Geocode;
pub use hotel_reviews::HotelReviews;
pub use map::{GenerateCityMap, GenerateMap};
pub use mode::SetMode;
pub use phase::SetPhase;
pub use route_check::RouteCheck;
pub use search_web::SearchWeb;
pub use trains::SearchTrains;
pub use update_notes::UpdateNotes;
pub use weather::GetWeather;
pub use xhs::{ReadXhsNote, SearchXhs};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::Value;

use crate::llm;

/// 工具 trait：各工具实现它，注册进 agent。
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    async fn execute(&self, args: Value) -> Result<String>;
}

/// 带 Send+Sync 的 trait 对象类型（多线程 runtime 需要 future 是 Send）。
pub type DynTool = dyn Tool + Send + Sync;

/// 把一个 Tool 转成 LLM 请求里的工具定义。
pub fn llm_tool(t: &DynTool) -> llm::Tool {
    llm::Tool {
        kind: "function".into(),
        function: llm::ToolFunction {
            name: t.name().into(),
            description: t.description().into(),
            parameters: t.parameters(),
        },
    }
}

/// 解析工具调用参数（arguments 是 JSON 字符串）。
pub fn parse_args(raw: Option<&str>, tool_name: &str) -> Result<Value> {
    let s = raw.unwrap_or("{}");
    serde_json::from_str(s).with_context(|| format!("解析工具 {tool_name} 参数失败: {s}"))
}

/// 两经纬点球面距离（km）。cluster_pois / 地图工具共用。
pub(crate) fn dist_km(lon1: f64, lat1: f64, lon2: f64, lat2: f64) -> f64 {
    let r = 6371.0_f64;
    let la1 = lat1.to_radians();
    let la2 = lat2.to_radians();
    let dla = la2 - la1;
    let dlo = (lon2 - lon1).to_radians();
    let a = (dla / 2.0).sin().powi(2) + la1.cos() * la2.cos() * (dlo / 2.0).sin().powi(2);
    2.0 * r * a.sqrt().asin()
}

/// 单链接聚类原语（cluster_pois 与城市图簇放大图共用）：n 个点、两两距离函数、
/// 阈值 t（d ≤ t 归同簇，链式传递）。返回各簇成员下标（簇内升序，簇间按首成员升序）。
pub(crate) fn single_link_clusters(
    n: usize,
    dist: impl Fn(usize, usize) -> f64,
    t: f64,
) -> Vec<Vec<usize>> {
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(p: &mut Vec<usize>, i: usize) -> usize {
        if p[i] != i {
            p[i] = find(p, p[i]);
        }
        p[i]
    }
    for i in 0..n {
        for j in (i + 1)..n {
            if dist(i, j) <= t {
                let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                parent[ri] = rj;
            }
        }
    }
    let mut clusters: std::collections::HashMap<usize, Vec<usize>> =
        std::collections::HashMap::new();
    for i in 0..n {
        clusters.entry(find(&mut parent, i)).or_default().push(i);
    }
    let mut out: Vec<Vec<usize>> = clusters.into_values().collect();
    out.sort_by_key(|m| m[0]);
    out
}

/// 高德 API 共享限流器：所有请求发前 [`RateLimiter::wait`]，保证相邻两次请求间隔 ≥ min_interval。
/// agent 内工具串行执行，单进程内全局 400ms 间隔 ≈ 2.5 QPS，低于个人 key 上限（~3 QPS）。
/// 仅本进程内生效；多进程共用同一 key 时靠各请求点的 QPS 报错重试兜底。
#[derive(Clone)]
pub struct RateLimiter {
    inner: std::sync::Arc<RateLimiterInner>,
}

struct RateLimiterInner {
    last: tokio::sync::Mutex<Option<std::time::Instant>>,
    min_interval: std::time::Duration,
}

impl RateLimiter {
    pub fn new(min_interval_ms: u64) -> Self {
        Self {
            inner: std::sync::Arc::new(RateLimiterInner {
                last: tokio::sync::Mutex::new(None),
                min_interval: std::time::Duration::from_millis(min_interval_ms),
            }),
        }
    }

    /// 发请求前调用：距上次请求不足间隔时 sleep 补齐。
    /// 持锁睡眠——并发的后续调用会在这里自动排队，依次拿到间隔 ≥ min_interval 的槽位。
    pub async fn wait(&self) {
        let mut last = self.inner.last.lock().await;
        if let Some(t) = *last {
            let elapsed = t.elapsed();
            if elapsed < self.inner.min_interval {
                tokio::time::sleep(self.inner.min_interval - elapsed).await;
            }
        }
        *last = Some(std::time::Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 限流器间隔：首次立即放行，之后每次间隔 ≥ min_interval。
    #[tokio::test]
    async fn rate_limiter_spacing() {
        let rl = RateLimiter::new(50);
        let t0 = std::time::Instant::now();
        rl.wait().await; // 首次无记录，立即放行
        assert!(t0.elapsed() < std::time::Duration::from_millis(50));
        rl.wait().await;
        rl.wait().await;
        // 2、3 次之间各隔 ≥50ms
        assert!(
            t0.elapsed() >= std::time::Duration::from_millis(100),
            "3 次请求总耗时应 ≥ 2×50ms，实际 {:?}",
            t0.elapsed()
        );
    }

    /// 单链接聚类原语：链式传递、簇内/簇间升序（确定性）、空输入返回空。
    #[test]
    fn single_link_clusters_basic() {
        // 一维点：0,1,2 一簇（间隔 1），10,11 一簇（间隔 1），间距 10 分开
        let pts: [f64; 5] = [0.0, 1.0, 2.0, 10.0, 11.0];
        let dist = |i: usize, j: usize| (pts[i] - pts[j]).abs();
        let clusters = single_link_clusters(pts.len(), dist, 1.5);
        assert_eq!(clusters, vec![vec![0, 1, 2], vec![3, 4]]);
        // 阈值收紧 → 各自成簇
        let single = single_link_clusters(pts.len(), dist, 0.5);
        assert_eq!(single, vec![vec![0], vec![1], vec![2], vec![3], vec![4]]);
        // 链式传递：0-1 距离 2，1-2 距离 2，0-2 距离 4；t=2 → 0,1,2 一簇
        let chain: [f64; 3] = [0.0, 2.0, 4.0];
        let linked = single_link_clusters(3, |i, j| (chain[i] - chain[j]).abs(), 2.0);
        assert_eq!(linked, vec![vec![0, 1, 2]]);
        assert!(single_link_clusters(0, |_, _| 0.0, 1.0).is_empty());
    }
}
