//! POI 几何聚类工具（cluster_pois）：纯几何计算，零网络请求。
//! 阶段2 大局规划用：把 LLM 搜好并 geocode 的景点/枢纽坐标聚成
//! 「交通便利区（part 核心候选）+ 散点挂靠」，为 part 划分提供数据支撑。
//!
//! 两段式防桥接（确定性算法，结果可复现）：
//! ① 每点算最近邻距离 nn，阈值 T = clamp(median(nn)×2.5, 5km, 25km)
//! ② nn ≤ T 的密集点单链接聚簇（交通便利区核心候选）
//! ③ anchors 先验优先：距 anchor ≤30km 的点直接归该区，附近小簇整体并入
//! ④ 散点（nn > T）只挂靠不入核——连到所有 d ≤ 2×dmin 的区（多归属），
//!    距所有区 >150km 为远散点（候选独立枢纽/独立景区）；零散景点结构上当不了桥
//! ⑤ min_regions 不满足 → 用 T×0.6 重聚一次，两版结果都输出，由 LLM/用户裁决

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use super::{Tool, dist_km, single_link_clusters};

/// 阈值夹限（km）：T = clamp(median(nn) × 2.5, T_MIN, T_MAX)
const T_MIN: f64 = 5.0;
const T_MAX: f64 = 25.0;
/// 阈值系数：T = median(nn) × NN_FACTOR
const NN_FACTOR: f64 = 2.5;
/// anchor 吸收半径（km）：距 anchor ≤ 此距离的点直接归该区
const ANCHOR_R: f64 = 30.0;
/// 散点到最近区中心超过此距离（km）→ 远散点（候选独立枢纽/独立景区）
const FAR_KM: f64 = 150.0;
/// 挂靠判定：到区中心 d ≤ 2 × dmin 的区都算挂靠候选（多归属）
const ATTACH_FACTOR: f64 = 2.0;

pub struct ClusterPois;

/// LLM 传入的已 geocode 点位。
#[derive(Deserialize)]
struct Poi {
    name: String,
    lon: f64,
    lat: f64,
}

/// 工具入参（anchors/min_regions 可选）。
#[derive(Deserialize)]
struct Args {
    pois: Vec<Poi>,
    #[serde(default)]
    anchors: Vec<Anchor>,
    #[serde(default)]
    min_regions: Option<usize>,
}

/// anchor：LLM 预判断的交通便利区中心（geocode 过的市区坐标）。
#[derive(Deserialize)]
struct Anchor {
    name: String,
    lon: f64,
    lat: f64,
}

/// 聚类一版结果。
struct ClusterOut {
    t: f64,
    /// 交通便利区候选：anchor 区（name=anchor 名）或自动命名簇
    regions: Vec<Region>,
    /// 散点挂靠：每点到各候选区的距离
    attached: Vec<AttachRow>,
    /// 远散点（候选独立枢纽/独立景区）
    far: Vec<FarPoint>,
}

struct Region {
    name: String,
    center: (f64, f64),
    /// 区内点（含 anchor 吸收的散点与并入的小簇成员）
    members: Vec<String>,
    from_anchor: bool,
}

struct AttachRow {
    name: String,
    /// (区名, 距离km)，按距离升序；多个 = 多归属候选
    candidates: Vec<(String, f64)>,
}

/// 远散点：距所有区 >150km；无任何区时 nearest 为 None（完全孤立点位）。
struct FarPoint {
    name: String,
    nearest: Option<(String, f64)>,
}

#[async_trait]
impl Tool for ClusterPois {
    fn name(&self) -> &str {
        "cluster_pois"
    }
    fn description(&self) -> &str {
        "纯几何聚类（零网络请求）：把已 geocode 的景点/枢纽坐标聚成「交通便利区（part 核心）\
         候选 + 散点挂靠」，为划分 part 提供数据支撑。\
         调用前请先自行做大白判断（预期几个交通便利区、大致在哪），\
         把预期区中心作为 anchors、预期最少区数作为 min_regions 传入；\
         结果不符预期（如两个市被并成一区）时，调整 anchors/min_regions 重调本工具，不要硬掰结果。"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pois": {
                    "type": "array",
                    "description": "已 geocode 的景点/枢纽坐标列表",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": {"type": "string"},
                            "lon": {"type": "number"},
                            "lat": {"type": "number"}
                        },
                        "required": ["name", "lon", "lat"]
                    }
                },
                "anchors": {
                    "type": "array",
                    "description": "可选，预判断的交通便利区中心（geocode 过的市区坐标），距其 ≤30km 的点直接归该区",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": {"type": "string"},
                            "lon": {"type": "number"},
                            "lat": {"type": "number"}
                        },
                        "required": ["name", "lon", "lat"]
                    }
                },
                "min_regions": {
                    "type": "integer",
                    "description": "可选，预期最少交通便利区数；不满足时自动用更紧阈值重聚一次，两版结果都输出"
                }
            },
            "required": ["pois"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String> {
        let args: Args =
            serde_json::from_value(args).context("解析 cluster_pois 参数失败（需 pois 数组）")?;
        let pois = &args.pois;
        let anchors = &args.anchors;

        if pois.is_empty() {
            return Ok("聚类失败：pois 为空".into());
        }
        // anchor 名是区的唯一标识（输出表格/散点候选都引用它），重名会导致归属混乱
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for a in anchors {
            if !seen.insert(a.name.as_str()) {
                return Ok(format!("聚类失败：anchors 名称「{}」重复", a.name));
            }
        }
        if pois.len() == 1 {
            let p = &pois[0];
            return Ok(format!(
                "仅 1 个点「{}」({:.4},{:.4})，无法聚类；该点按孤立点位处理，请结合攻略判断归属。",
                p.name, p.lon, p.lat
            ));
        }

        let first = run_cluster(pois, anchors, threshold(pois));
        let mut text = render_version(&first, "聚类结果");
        // min_regions 校验：不满足 → 更紧阈值重聚一次，两版都输出
        if let Some(min_r) = args.min_regions {
            let n_regions = first.regions.len();
            if n_regions < min_r {
                let t2 = (first.t * 0.6).max(1.0);
                let second = run_cluster(pois, anchors, t2);
                text.push_str(&format!(
                    "\n⚠ 预期至少 {min_r} 个交通便利区，第一版仅聚出 {n_regions} 个；\
                     已用更紧阈值 T'={t2:.1}km 重聚一次，两版结果供对照裁决：\n"
                ));
                text.push_str(&render_version(&second, "第二版（更紧阈值）"));
            }
        }
        text.push_str(
            "\n提示：以上为几何结果。请结合 route_check 打车时长抽查与攻略口径（地铁/「市区」）\
             确认后输出 part 划分表；多归属点按攻略归一个 part，展示时提一嘴「介于X与Y之间」即可。\n",
        );
        Ok(text)
    }
}

/// 每点到最近其他点的距离。
fn nearest_neighbor_dists(pois: &[Poi]) -> Vec<f64> {
    pois.iter()
        .map(|p| {
            pois.iter()
                .filter(|q| q.name != p.name || q.lon != p.lon || q.lat != p.lat)
                .map(|q| dist_km(p.lon, p.lat, q.lon, q.lat))
                .fold(f64::INFINITY, f64::min)
        })
        .collect()
}

/// 阈值 T = clamp(median(nn) × 2.5, 5, 25)。
fn threshold(pois: &[Poi]) -> f64 {
    let mut nns = nearest_neighbor_dists(pois);
    nns.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = nns.len();
    let median = if n % 2 == 1 {
        nns[n / 2]
    } else {
        (nns[n / 2 - 1] + nns[n / 2]) / 2.0
    };
    (median * NN_FACTOR).clamp(T_MIN, T_MAX)
}

/// 执行一版聚类（给定阈值 T）。
fn run_cluster(pois: &[Poi], anchors: &[Anchor], t: f64) -> ClusterOut {
    let nns = nearest_neighbor_dists(pois);
    // 核心候选（nn ≤ T）单链接聚簇；散点（nn > T）只挂靠
    let core: Vec<usize> = (0..pois.len()).filter(|&i| nns[i] <= t).collect();
    // 下标映射回 poi 原下标；簇内/簇间均升序（确定性输出）
    let cluster_list: Vec<Vec<usize>> = single_link_clusters(
        core.len(),
        |a, b| {
            dist_km(
                pois[core[a]].lon,
                pois[core[a]].lat,
                pois[core[b]].lon,
                pois[core[b]].lat,
            )
        },
        t,
    )
    .into_iter()
    .map(|m| m.into_iter().map(|ci| core[ci]).collect())
    .collect();

    // anchor 区：≤ANCHOR_R 的点直接归该区；附近小簇（最近成员 ≤ANCHOR_R）整体并入
    let mut regions: Vec<Region> = anchors
        .iter()
        .map(|a| Region {
            name: a.name.clone(),
            center: (a.lon, a.lat),
            members: Vec::new(),
            from_anchor: true,
        })
        .collect();
    let mut absorbed: Vec<bool> = vec![false; pois.len()];
    // 点位按最近 anchor 吸收（距多个 anchor 都 ≤30km 时只归最近者，避免重复计入）
    for (pi, p) in pois.iter().enumerate() {
        let nearest = anchors
            .iter()
            .enumerate()
            .filter(|(_, a)| dist_km(a.lon, a.lat, p.lon, p.lat) <= ANCHOR_R)
            .min_by(|(_, a1), (_, a2)| {
                let d1 = dist_km(a1.lon, a1.lat, p.lon, p.lat);
                let d2 = dist_km(a2.lon, a2.lat, p.lon, p.lat);
                d1.partial_cmp(&d2).unwrap_or(std::cmp::Ordering::Equal)
            });
        if let Some((ai, _)) = nearest {
            regions[ai].members.push(p.name.clone());
            absorbed[pi] = true;
        }
    }
    // 小簇并入最近的 anchor（簇内任一未吸收成员距 anchor ≤ ANCHOR_R 即视为附近）。
    // 只补录未吸收成员——成员已按点吸收过的簇跳过，防止区内点重复计入
    for members in &cluster_list {
        if members.iter().all(|&pi| absorbed[pi]) {
            continue;
        }
        let Some(first) = members.first() else {
            continue;
        };
        let nearest = anchors.iter().enumerate().min_by(|(_, a1), (_, a2)| {
            let d1 = dist_km(a1.lon, a1.lat, pois[*first].lon, pois[*first].lat);
            let d2 = dist_km(a2.lon, a2.lat, pois[*first].lon, pois[*first].lat);
            d1.partial_cmp(&d2).unwrap_or(std::cmp::Ordering::Equal)
        });
        let Some((ai, a)) = nearest else {
            continue;
        };
        if members.iter().any(|&pi| {
            !absorbed[pi] && dist_km(a.lon, a.lat, pois[pi].lon, pois[pi].lat) <= ANCHOR_R
        }) {
            for &pi in members {
                if !absorbed[pi] {
                    regions[ai].members.push(pois[pi].name.clone());
                    absorbed[pi] = true;
                }
            }
        }
    }
    // 未并入 anchor 的簇 → 独立区（中心 = 成员质心，名字取首个成员）
    for members in &cluster_list {
        if members.iter().all(|&pi| absorbed[pi]) {
            continue;
        }
        let alive: Vec<usize> = members
            .iter()
            .copied()
            .filter(|&pi| !absorbed[pi])
            .collect();
        if alive.is_empty() {
            continue;
        }
        let n = alive.len() as f64;
        let cy = alive.iter().map(|&pi| pois[pi].lat).sum::<f64>() / n;
        let cx = alive.iter().map(|&pi| pois[pi].lon).sum::<f64>() / n;
        regions.push(Region {
            name: format!("{}一带", pois[alive[0]].name),
            center: (cx, cy),
            members: alive.iter().map(|&pi| pois[pi].name.clone()).collect(),
            from_anchor: false,
        });
    }

    // 散点挂靠：到各区中心的距离，d ≤ 2×dmin 的都列为候选；dmin > FAR_KM 为远散点
    let mut attached = Vec::new();
    let mut far = Vec::new();
    for (pi, p) in pois.iter().enumerate() {
        if absorbed[pi] || nns[pi] <= t {
            continue;
        }
        let mut ds: Vec<(String, f64)> = regions
            .iter()
            .map(|r| {
                (
                    r.name.clone(),
                    dist_km(p.lon, p.lat, r.center.0, r.center.1),
                )
            })
            .collect();
        ds.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        let Some((rn, dmin)) = ds.first().map(|(n, d)| (n.clone(), *d)) else {
            // 无任何区（无 anchor 且无簇）：完全孤立点位
            far.push(FarPoint {
                name: p.name.clone(),
                nearest: None,
            });
            continue;
        };
        if dmin > FAR_KM {
            far.push(FarPoint {
                name: p.name.clone(),
                nearest: Some((rn, dmin)),
            });
            continue;
        }
        let candidates: Vec<(String, f64)> = ds
            .into_iter()
            .take_while(|(_, d)| *d <= dmin * ATTACH_FACTOR + 1e-9)
            .collect();
        attached.push(AttachRow {
            name: p.name.clone(),
            candidates,
        });
    }
    ClusterOut {
        t,
        regions,
        attached,
        far,
    }
}

/// 渲染一版聚类结果为结构化文本。
fn render_version(out: &ClusterOut, title: &str) -> String {
    let mut text = format!("{title}（阈值 T={:.1}km）：\n\n", out.t);
    text.push_str("## 交通便利区候选（part 核心）\n");
    for (i, r) in out.regions.iter().enumerate() {
        let kind = if r.from_anchor { "anchor" } else { "聚类簇" };
        text.push_str(&format!(
            "{}. {}（{kind}）中心({:.4},{:.4})\n   区内点：{}\n",
            i + 1,
            r.name,
            r.center.0,
            r.center.1,
            if r.members.is_empty() {
                "（无，anchor 即市区中心）".to_string()
            } else {
                r.members.join("、")
            }
        ));
    }
    text.push_str("\n## 散点挂靠（到各候选区距离，最近者标 ●）\n");
    if out.attached.is_empty() {
        text.push_str("（无）\n");
    }
    for row in &out.attached {
        let parts: Vec<String> = row
            .candidates
            .iter()
            .enumerate()
            .map(|(i, (n, d))| {
                let mark = if i == 0 { "●" } else { "" };
                format!("{n} {d:.0}km{mark}")
            })
            .collect();
        text.push_str(&format!("- {} → {}\n", row.name, parts.join("、")));
    }
    text.push_str(&format!(
        "\n## 远散点（距所有区 >{FAR_KM:.0}km，候选独立枢纽/独立景区）\n"
    ));
    if out.far.is_empty() {
        text.push_str("（无）\n");
    }
    for fp in &out.far {
        match &fp.nearest {
            Some((region, d)) => {
                text.push_str(&format!("- {}（最近区 {region}，{d:.0}km）\n", fp.name))
            }
            None => text.push_str(&format!("- {}（附近无任何区，完全孤立点位）\n", fp.name)),
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 陕北场景：延安/榆林两市 + 中间散点（波浪谷多归属、壶口单归属、银川远散点）。
    /// 断言两市不被零散景点桥接成一个区，散点挂靠关系正确。
    #[tokio::test]
    async fn two_cities_not_bridged() {
        let args = serde_json::json!({
            "pois": [
                {"name": "宝塔山", "lon": 109.49, "lat": 36.60},
                {"name": "延安革命纪念馆", "lon": 109.47, "lat": 36.62},
                {"name": "镇北台", "lon": 109.75, "lat": 38.30},
                {"name": "榆林老街", "lon": 109.76, "lat": 38.28},
                {"name": "波浪谷", "lon": 108.85, "lat": 37.58},
                {"name": "壶口瀑布", "lon": 110.47, "lat": 36.13},
                {"name": "沙坡头", "lon": 105.19, "lat": 37.50}
            ],
            "anchors": [
                {"name": "延安市区", "lon": 109.49, "lat": 36.60},
                {"name": "榆林市区", "lon": 109.75, "lat": 38.29}
            ]
        });
        let tool = ClusterPois;
        let out = tool.execute(args).await.expect("execute 失败");
        assert!(out.contains("延安市区"), "缺 anchor 区:\n{out}");
        assert!(out.contains("榆林市区"), "缺 anchor 区:\n{out}");
        // 防桥接：两市各自成区，中间散点没有把它们并起来
        assert_eq!(
            out.matches("（anchor）").count(),
            2,
            "anchor 区数应为2:\n{out}"
        );
        // 波浪谷多归属（延安+榆林都在 2×dmin 内）
        let blg_row = out
            .lines()
            .find(|l| l.contains("波浪谷"))
            .expect("波浪谷行");
        assert!(
            blg_row.contains("延安") && blg_row.contains("榆林"),
            "波浪谷应多归属: {blg_row}"
        );
        // 壶口瀑布最近为延安
        let hk_row = out
            .lines()
            .find(|l| l.contains("壶口瀑布"))
            .expect("壶口行");
        assert!(hk_row.contains("延安"), "壶口应挂靠延安: {hk_row}");
        // 沙坡头距所有区 >150km → 远散点
        assert!(
            out.contains("远散点") && out.contains("沙坡头"),
            "沙坡头应为远散点:\n{out}"
        );
    }

    /// 稀疏链桥接防护：两市之间一串间隔 30km 的点，不会被串成一个区。
    #[tokio::test]
    async fn sparse_chain_cannot_bridge() {
        let args = serde_json::json!({
            "pois": [
                {"name": "A城景点1", "lon": 109.0, "lat": 36.0},
                {"name": "A城景点2", "lon": 109.2, "lat": 36.0},
                {"name": "链点1", "lon": 109.5, "lat": 36.9},
                {"name": "链点2", "lon": 109.8, "lat": 37.8},
                {"name": "链点3", "lon": 110.1, "lat": 38.7},
                {"name": "B城景点1", "lon": 110.4, "lat": 39.6},
                {"name": "B城景点2", "lon": 110.6, "lat": 39.6}
            ],
            "anchors": [
                {"name": "A城", "lon": 109.1, "lat": 36.0},
                {"name": "B城", "lon": 110.5, "lat": 39.6}
            ]
        });
        let tool = ClusterPois;
        let out = tool.execute(args).await.expect("execute 失败");
        assert_eq!(
            out.matches("（anchor）").count(),
            2,
            "两市应各自成区:\n{out}"
        );
        // 链点不会让两市合并——只在散点挂靠段出现
        assert!(out.contains("散点挂靠"), "应有散点段:\n{out}");
    }

    /// min_regions 校验：第一版 1 个区 < 预期 2 → 自动用更紧阈值重聚并输出两版。
    #[tokio::test]
    async fn min_regions_retry() {
        let args = serde_json::json!({
            "pois": [
                {"name": "p1", "lon": 109.0, "lat": 36.0},
                {"name": "p2", "lon": 109.0, "lat": 36.08},
                {"name": "p3", "lon": 109.0, "lat": 36.24},
                {"name": "p4", "lon": 109.0, "lat": 36.32}
            ],
            "min_regions": 2
        });
        let tool = ClusterPois;
        let out = tool.execute(args).await.expect("execute 失败");
        assert!(out.contains("⚠"), "应输出警告:\n{out}");
        assert!(out.contains("第二版"), "应输出第二版:\n{out}");
    }

    /// 回归：anchor 按点吸收后，附近小簇并入不得重复计入同一成员（郑州实测曾复现）。
    #[tokio::test]
    async fn anchor_absorb_no_duplicate_members() {
        let args = serde_json::json!({
            "pois": [
                {"name": "市区A", "lon": 109.0, "lat": 36.0},
                {"name": "市区B", "lon": 109.05, "lat": 36.0},
                {"name": "远点", "lon": 109.28, "lat": 36.0}
            ],
            "anchors": [{"name": "市区", "lon": 109.0, "lat": 36.0}]
        });
        let out = ClusterPois.execute(args).await.expect("execute 失败");
        for line in out.lines().filter(|l| l.contains("区内点")) {
            assert!(line.matches("市区A").count() <= 1, "成员重复: {line}");
            assert!(line.matches("市区B").count() <= 1, "成员重复: {line}");
        }
        // 远点 25km ≤ 30km 已被按点吸收，不应再出现在散点挂靠段
        assert!(!out.contains("- 远点 →"), "已吸收点不应挂靠:\n{out}");
    }

    /// 纯散点（无密集核心）+ 无 anchor：所有点各自挂靠/独立，不 panic、输出完整。
    #[tokio::test]
    async fn all_scattered_no_anchor() {
        let args = serde_json::json!({
            "pois": [
                {"name": "甲", "lon": 100.0, "lat": 30.0},
                {"name": "乙", "lon": 102.0, "lat": 32.0},
                {"name": "丙", "lon": 104.0, "lat": 34.0}
            ]
        });
        let tool = ClusterPois;
        let out = tool.execute(args).await.expect("execute 失败");
        assert!(out.contains("散点挂靠"), "应输出散点段:\n{out}");
        assert!(out.contains("甲") && out.contains("乙") && out.contains("丙"));
    }
}
