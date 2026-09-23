//! 总览图工具（generate_overview_map）：白底 part 化模式图。
//! 交通便利区（part 核心）画「意会圈」（虚线圆，半径由区内点最远距离确定），
//! 偏远辐射景点画细虚线连到区圈边（多归属都画），part 间大交通为彩色条纹线。
//! 自绘渲染（image/imageproc/ab_glyph），不联网。产物 maps/{session_id}/overview_{n}.png。
//!
//! 原则：LLM 给事实数据（坐标/归属/大交通），工具确定性定半径与画法。

use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result};
use async_trait::async_trait;
use image::{Rgba, RgbaImage};
use imageproc::drawing::{
    draw_filled_circle_mut, draw_filled_rect_mut, draw_hollow_circle_mut, draw_text_mut, text_size,
};
use imageproc::rect::Rect;
use serde::Deserialize;
use serde_json::Value;

use super::Tool;
use super::{
    LegendItem, LineStyle, MARKER_RING, RouteSeg, draw_flow_legend, draw_thick_line, sanitize,
};
use crate::tools::{dist_km, single_link_clusters};

pub struct GenerateMap {
    font_path: String,
    session_id: String,
    counter: AtomicU32,
}

impl GenerateMap {
    /// counter 初值从 maps/{session_id}/ 已有图片的最大编号续接（重进旧会话不覆盖旧图）。
    pub fn new(font_path: String, session_id: String) -> Self {
        Self {
            font_path,
            counter: AtomicU32::new(super::scan_max_index(&session_id, "overview")),
            session_id,
        }
    }

    /// 懒加载 CJK 字体（共用实现见 mod.rs）。
    fn load_font(&self) -> Option<ab_glyph::FontVec> {
        super::load_font(&self.font_path)
    }
}

/// 交通便利区中心（part 核心，geocode 过的市区/县城坐标）。
#[derive(Deserialize, Clone)]
struct Hub {
    name: String,
    lon: f64,
    lat: f64,
}

/// 标注点类型（决定标记样式）。
#[derive(Clone, Copy, PartialEq, Debug)]
enum PointKind {
    /// 火车站：实心方块
    Station,
    /// 机场：叉号
    Airport,
    /// 景点：实心圆点
    Scenic,
    /// 普通城市（出发地/目的地等）：空心圆环
    City,
}

/// LLM 传入的标注点（区内点或辐射景点）。
#[derive(Deserialize, Clone)]
struct PointNode {
    name: String,
    lon: f64,
    lat: f64,
    /// 类型：station/airport/scenic/city（默认 scenic）
    #[serde(rename = "type", default)]
    point_type: Option<String>,
    /// 显式辐射归属（hub 名列表，多个 = 多归属都画线）；不传/空则按距离自动挂靠
    #[serde(default)]
    hubs: Option<Vec<String>>,
}

/// 工具入参（points/routes 可省略）。
#[derive(Deserialize)]
struct Args {
    hubs: Vec<Hub>,
    #[serde(default)]
    points: Vec<PointNode>,
    #[serde(default)]
    routes: Vec<RouteSeg>,
}

/// 轮换调色板（高对比度）。
const PALETTE: [Rgba<u8>; 8] = [
    Rgba([220, 38, 38, 255]),  // 红
    Rgba([37, 99, 235, 255]),  // 蓝
    Rgba([22, 163, 74, 255]),  // 绿
    Rgba([249, 115, 22, 255]), // 橙
    Rgba([147, 51, 234, 255]), // 紫
    Rgba([13, 148, 136, 255]), // 青绿
    Rgba([180, 83, 9, 255]),   // 棕
    Rgba([236, 72, 153, 255]), // 粉
];

/// 白色光晕色 + 圆心点色（MARKER_RING 见 mod.rs 共用）。
const MARKER_CORE: Rgba<u8> = Rgba([30, 41, 59, 255]);
const HALO: Rgba<u8> = Rgba([255, 255, 255, 255]);
/// 浅灰底色 + 网格线色 + 经纬度标注色。
const BG: Rgba<u8> = Rgba([248, 250, 252, 255]);
const GRID: Rgba<u8> = Rgba([186, 194, 206, 255]); // slate-400
const LONLAT_LABEL: Rgba<u8> = Rgba([100, 116, 139, 255]); // slate-500
/// 辐射细虚线色。
const REMOTE_LINE: Rgba<u8> = Rgba([100, 116, 139, 255]); // slate-500

/// 圈内/偏远分界：点到 hub 距离 ≤ 此值（km）算区内点，参与圈半径计算。
const T_NEAR_KM: f64 = 20.0;
/// 意会圈半径 = 区内点最远距离 × 此系数，下限 RADIUS_MIN_KM、上限 RADIUS_CAP_KM。
const RADIUS_FACTOR: f64 = 1.2;
const RADIUS_MIN_KM: f64 = 4.0;
const RADIUS_CAP_KM: f64 = 60.0;

/// 解析点位类型（兼容中文别名）。
fn parse_point_type(s: Option<&str>) -> PointKind {
    match s.unwrap_or("") {
        "station" | "火车站" | "车站" | "高铁站" => PointKind::Station,
        "airport" | "机场" => PointKind::Airport,
        "city" | "城市" | "出发地" | "目的地" => PointKind::City,
        _ => PointKind::Scenic,
    }
}

/// 各交通便利区的意会圈半径（km）：区内点（d ≤ T_NEAR）最远距离 × 1.2，下限 4km。
fn hub_radii(hubs: &[Hub], points: &[PointNode]) -> Vec<f64> {
    hubs.iter()
        .map(|h| {
            let inner_max = points
                .iter()
                .map(|p| dist_km(h.lon, h.lat, p.lon, p.lat))
                .filter(|d| *d <= T_NEAR_KM)
                .fold(0.0_f64, f64::max);
            (inner_max * RADIUS_FACTOR).clamp(RADIUS_MIN_KM, RADIUS_CAP_KM)
        })
        .collect()
}

/// 偏远点的辐射目标 hub 下标：显式 hubs 优先（空数组视为未传）；否则所有 d ≤ 2×dmin 的 hub（多归属）。
fn remote_targets(point: &PointNode, hubs: &[Hub]) -> Vec<usize> {
    if let Some(names) = &point.hubs
        && !names.is_empty()
    {
        return names
            .iter()
            .filter_map(|n| hubs.iter().position(|h| &h.name == n))
            .collect();
    }
    let mut ds: Vec<(usize, f64)> = hubs
        .iter()
        .enumerate()
        .map(|(i, h)| (i, dist_km(h.lon, h.lat, point.lon, point.lat)))
        .collect();
    ds.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    let Some(dmin) = ds.first().map(|(_, d)| *d) else {
        return Vec::new();
    };
    ds.into_iter()
        .take_while(|(_, d)| *d <= dmin * 2.0 + 1e-9)
        .map(|(i, _)| i)
        .collect()
}

#[async_trait]
impl Tool for GenerateMap {
    fn name(&self) -> &str {
        "generate_overview_map"
    }
    fn description(&self) -> &str {
        "生成白底模式图。两种用法：\
         ① 总览图（part 化）：hubs=交通便利区中心（part 核心），画意会圈+辐射景点+大交通连线；\
         ② 位置关系图（阶段1 候选展示）：hubs 传空数组，只传 points（type 用 city）列点，\
         展示候选地大致位置关系，不画圈不画交通。\
         points 可标 type 与多归属 hubs；routes=大交通（from/to 引用 hubs/points 的 name）。\
         意会圈半径由工具按区内点最远距离确定，无需指定。调用前请先 geocode。"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "hubs": {
                    "type": "array",
                    "description": "交通便利区中心列表（part 核心，geocode 过的市区/县城坐标）；阶段1 候选位置关系图传空数组",
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
                "points": {
                    "type": "array",
                    "description": "区内点/辐射景点列表（type 决定标记样式；偏远点可传 hubs 指定辐射归属，多个=多归属都画线）",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": {"type": "string"},
                            "lon": {"type": "number"},
                            "lat": {"type": "number"},
                            "type": {"type": "string", "description": "station|airport|scenic|city（火车站|机场|景点|普通城市），默认 scenic"},
                            "hubs": {"type": "array", "items": {"type": "string"}, "description": "可选，辐射归属的交通便利区名（多归属传多个）"}
                        },
                        "required": ["name", "lon", "lat"]
                    }
                },
                "routes": {
                    "type": "array",
                    "description": "part 间大交通路线，from/to 引用 hubs/points 里的 name",
                    "items": {
                        "type": "object",
                        "properties": {
                            "from": {"type": "string"},
                            "to": {"type": "string"},
                            "transport": {"type": "string", "description": "交通方式：高铁/飞机/自驾/大巴 等"}
                        },
                        "required": ["from", "to"]
                    }
                }
            },
            "required": ["hubs"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String> {
        let args: Args = serde_json::from_value(args)
            .context("解析 generate_overview_map 参数失败（需 hubs 数组）")?;
        let hubs = &args.hubs;
        let points = &args.points;
        let routes = &args.routes;

        if hubs.is_empty() && points.is_empty() {
            return Ok("地图生成失败：hubs 与 points 均为空（位置关系图需至少列出 points）".into());
        }
        // 名字唯一性：hubs ∪ points
        let mut names: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for h in hubs {
            if !names.insert(h.name.as_str()) {
                return Ok(format!("地图生成失败：名称「{}」重复", h.name));
            }
        }
        for p in points {
            if !names.insert(p.name.as_str()) {
                return Ok(format!("地图生成失败：名称「{}」重复", p.name));
            }
        }
        for (i, r) in routes.iter().enumerate() {
            if !names.contains(r.from.as_str()) {
                return Ok(format!(
                    "地图生成失败：routes[{i}].from「{}」不在 hubs/points 中",
                    r.from
                ));
            }
            if !names.contains(r.to.as_str()) {
                return Ok(format!(
                    "地图生成失败：routes[{i}].to「{}」不在 hubs/points 中",
                    r.to
                ));
            }
        }
        for p in points {
            if let Some(hs) = &p.hubs {
                for hn in hs {
                    if !hubs.iter().any(|h| &h.name == hn) {
                        return Ok(format!(
                            "地图生成失败：point「{}」的 hubs 引用了不存在的区「{}」",
                            p.name, hn
                        ));
                    }
                }
            }
        }

        self.render_overview(hubs, points, routes)
    }
}

impl GenerateMap {
    /// 落盘主图 + 打包组聚焦放大图（overview_{n}_c{k}.png，同 city 图簇放大图惯例）。
    /// 编号在主图成功后才消耗；放大图失败不阻塞主图。
    fn render_overview(
        &self,
        hubs: &[Hub],
        points: &[PointNode],
        routes: &[RouteSeg],
    ) -> Result<String> {
        let (img, packed) = self.render_scene(hubs, points, routes, "图例", true)?;
        let dir = std::path::Path::new("maps").join(sanitize(&self.session_id));
        std::fs::create_dir_all(&dir).context("创建 maps 目录失败")?;
        let n = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
        let main_path = dir.join(format!("overview_{n}.png"));
        img.save(&main_path)
            .with_context(|| format!("保存 {} 失败", main_path.display()))?;

        let mut lines = vec![if hubs.is_empty() {
            format!(
                "已生成位置关系图：{}（{} 个点位，仅展示大致位置关系）",
                main_path.display(),
                points.len()
            )
        } else {
            format!(
                "已生成总览图：{}（{} 个交通便利区、{} 个标注点、{} 条大交通）",
                main_path.display(),
                hubs.len(),
                points.len(),
                routes.len()
            )
        }];
        for (k, g) in packed.iter().take(MAX_SUB_MAPS).enumerate() {
            match self.render_scene(
                &[],
                &g.members,
                &[],
                &format!("图例 · {}", g.summary),
                false,
            ) {
                Ok((sub, _)) => {
                    let p = dir.join(format!("overview_{n}_c{}.png", k + 1));
                    if sub.save(&p).is_ok() {
                        lines.push(format!("放大图：{}（{}）", p.display(), g.summary));
                    }
                }
                Err(e) => tracing::warn!("overview 放大图生成失败（{}）：{e}", g.summary),
            }
        }
        Ok(lines.join("\n"))
    }
}

/// 打包组：概述 + 聚焦成员（hub 成员转为 city 点位，纯点位子图渲染）。
struct PackedGroup {
    summary: String,
    members: Vec<PointNode>,
}

/// 每张主图最多追加的聚焦放大图数（同 city 图簇放大图上限）。
const MAX_SUB_MAPS: usize = 3;

impl GenerateMap {
    /// 渲染一张白底模式图（不落盘）。allow_pack=false 时所有标签画全名（放大图用，防递归打包）。
    fn render_scene(
        &self,
        hubs: &[Hub],
        points: &[PointNode],
        routes: &[RouteSeg],
        legend_title: &str,
        allow_pack: bool,
    ) -> Result<(RgbaImage, Vec<PackedGroup>)> {
        // 画布尺寸
        let (w, h) = (1200_u32, 800_u32);
        let pad = 80_i32;
        let mut img = RgbaImage::from_pixel(w, h, BG);

        // 0. 图例条目提前构建：实测高度决定内容重心是否需要上移避让。
        //    条目按图上实际出现的内容裁剪（无 hubs 不显示区条目，无对应点位/辐射线不显示其条目）
        let has_kind = |k: PointKind| {
            points
                .iter()
                .any(|p| parse_point_type(p.point_type.as_deref()) == k)
        };
        let has_remote = !hubs.is_empty()
            && points.iter().any(|p| {
                hubs.iter()
                    .map(|hb| dist_km(hb.lon, hb.lat, p.lon, p.lat))
                    .fold(f64::INFINITY, f64::min)
                    > T_NEAR_KM
            });
        let mut items: Vec<LegendItem> = vec![LegendItem::Text(legend_title.into())];
        if hubs.is_empty() {
            // 位置关系图（阶段1 候选展示）：只有点位
            items.push(LegendItem::Marker("城市/候选地".into()));
        } else {
            items.push(LegendItem::Marker("交通便利区（圈=大致范围）".into()));
        }
        if has_kind(PointKind::Scenic) {
            items.push(LegendItem::Dot("景点".into()));
        }
        if has_kind(PointKind::Station) {
            items.push(LegendItem::Square("火车站".into()));
        }
        if has_kind(PointKind::Airport) {
            items.push(LegendItem::Cross("机场".into()));
        }
        if has_remote {
            items.push(LegendItem::Thin {
                color: REMOTE_LINE,
                text: "辐射关系".into(),
            });
        }
        // 字体只加载一次（标签矩形/图例测高/绘制共用）
        let font = self.load_font();

        // 1. 经纬度边界：hubs + points + 意会圈外扩范围
        let radii = hub_radii(hubs, points);
        let mut min_lon = f64::INFINITY;
        let mut max_lon = f64::NEG_INFINITY;
        let mut min_lat = f64::INFINITY;
        let mut max_lat = f64::NEG_INFINITY;
        let mut grow = |lon: f64, lat: f64| {
            min_lon = min_lon.min(lon);
            max_lon = max_lon.max(lon);
            min_lat = min_lat.min(lat);
            max_lat = max_lat.max(lat);
        };
        for (h, r) in hubs.iter().zip(&radii) {
            grow(h.lon, h.lat);
            let dlat = r / 110.574;
            let dlon = r / (111.32 * h.lat.to_radians().cos().abs().max(0.01));
            grow(h.lon - dlon, h.lat - dlat);
            grow(h.lon + dlon, h.lat + dlat);
        }
        for p in points {
            grow(p.lon, p.lat);
        }

        // 防 0 范围：给个最小跨度
        let lon_span = (max_lon - min_lon).max(0.01);
        let lat_span = (max_lat - min_lat).max(0.01);

        // 2. 等距圆柱投影，按重心纬度做 cos 修正减小 x 拉伸
        let lon0 = (min_lon + max_lon) / 2.0; // 重心经度
        let lat0 = (min_lat + max_lat) / 2.0; // 重心纬度
        let cos_lat = lat0.to_radians().cos().abs().max(0.01);
        let avail_w = (w as i32 - 2 * pad) as f64;
        let avail_h = (h as i32 - 2 * pad) as f64;
        let x_extent = lon_span * cos_lat;
        let scale = (avail_w / x_extent).min(avail_h / lat_span);

        // 重心映射到画布偏左上；图例通栏高时重心上移，保证内容下缘不压图例
        let cx_img = (w as f64) * 0.46;
        let cy_for = |band: f64| {
            ((h as f64) * 0.46)
                .min(h as f64 - band - avail_h / 2.0)
                .max(avail_h / 2.0 + 8.0)
        };
        let project = |lon: f64, lat: f64, cy: f64| -> (f32, f32) {
            (
                (cx_img + (lon - lon0) * cos_lat * scale) as f32,
                (cy - (lat - lat0) * scale) as f32,
            )
        };

        // 1.5 标签重叠打包：算每个标签矩形（与绘制同源摆放），矩形相交者归组（链式传递）。
        // 用临时重心检测——所有标签随重心统一竖直平移，相交关系与最终位置完全一致。
        let mut packed_summaries: Vec<(String, usize)> = Vec::new(); // (概述, 锚点下标)
        let mut packed_groups: Vec<PackedGroup> = Vec::new();
        let mut singles: Vec<usize> = Vec::new(); // 单独画全名的下标
        let mut entries: Vec<(bool, String, f64, f64)> = Vec::new(); // (是否hub, 名, lon, lat)
        if let Some(f) = &font {
            let cy0 = cy_for(0.0);
            for hb in hubs {
                entries.push((true, hb.name.clone(), hb.lon, hb.lat));
            }
            for p in points {
                entries.push((false, p.name.clone(), p.lon, p.lat));
            }
            let scale_hub = ab_glyph::PxScale::from(34.0);
            let scale_pt = ab_glyph::PxScale::from(26.0);
            let rects: Vec<(f32, f32, f32, f32)> = entries
                .iter()
                .map(|(is_hub, name, lon, lat)| {
                    let (x, y) = project(*lon, *lat, cy0);
                    label_rect(if *is_hub { scale_hub } else { scale_pt }, name, x, y, w, f)
                })
                .collect();
            for g in &group_overlaps(&rects) {
                if g.len() == 1 || !allow_pack {
                    // 单点画全名；放大图禁打包（防递归），全部画全名
                    for &i in g {
                        singles.push(i);
                    }
                } else {
                    let (summary, names, anchor) = pack_summary(&entries, g);
                    packed_summaries.push((summary.clone(), anchor));
                    // 明细进图例（flow legend 自动折行）
                    items.push(LegendItem::Text(format!("{summary}：{}", names.join("、"))));
                    // 聚焦成员：hub 转为 city 点位，普通点位保留原 type
                    let members = g
                        .iter()
                        .map(|&i| {
                            if i < hubs.len() {
                                let hb = &hubs[i];
                                PointNode {
                                    name: hb.name.clone(),
                                    lon: hb.lon,
                                    lat: hb.lat,
                                    point_type: Some("city".into()),
                                    hubs: None,
                                }
                            } else {
                                points[i - hubs.len()].clone()
                            }
                        })
                        .collect();
                    packed_groups.push(PackedGroup { summary, members });
                }
            }
        }
        // 大交通图例（在打包明细之后，顺序：标记说明 → 打包明细 → 大交通）
        for (i, r) in routes.iter().enumerate() {
            items.push(LegendItem::Swatch {
                color: PALETTE[i % PALETTE.len()],
                text: format!(
                    "{} → {} {}",
                    r.from,
                    r.to,
                    r.transport.as_deref().unwrap_or("")
                ),
            });
        }

        // 图例带（含打包明细行）→ 最终重心
        let band = match &font {
            Some(f) => super::flow_legend_band(&items, f, w as i32).max(0) as f64,
            None => 0.0,
        };
        let cy_img = cy_for(band);
        let to_px = |lon: f64, lat: f64| -> (f32, f32) { project(lon, lat, cy_img) };

        // 2.5 经纬度对齐网格 + 边缘标注（淡色背景层，铺满整张画布）
        // 步长按「像素间距」统一选档：两类线共用同一目标像素间距（内容长边 ~1/6），
        // 各轴各自向上取 1-2-5 整数度档——中纬度下经线不会比纬线密 cos_lat 倍，密度观感一致。
        let px_per_deg_lon = cos_lat * scale;
        let px_per_deg_lat = scale;
        let content_px_lon = lon_span * px_per_deg_lon;
        let content_px_lat = lat_span * px_per_deg_lat;
        let target_px = (content_px_lon.max(content_px_lat) / 6.0).clamp(90.0, 260.0);
        let lon_step = nice_step_for_px(target_px, px_per_deg_lon);
        let lat_step = nice_step_for_px(target_px, px_per_deg_lat);
        // 标注小数位随步长走（步长 <1° 时 {:.0} 会把多根线标成同一个整数度）
        let lon_decimals = if lon_step >= 1.0 {
            0
        } else if lon_step >= 0.1 {
            1
        } else {
            2
        };
        let lat_decimals = if lat_step >= 1.0 {
            0
        } else if lat_step >= 0.1 {
            1
        } else {
            2
        };
        let px_to_lon = |px: f64| -> f64 { lon0 + (px - cx_img) / (cos_lat * scale) };
        let px_to_lat = |py: f64| -> f64 { lat0 - (py - cy_img) / scale };
        let left_lon = px_to_lon(pad as f64);
        let right_lon = px_to_lon(w as f64 - pad as f64);
        let top_lat = px_to_lat(pad as f64);
        let bottom_lat = px_to_lat(h as f64 - pad as f64);
        let mut x_grid = (left_lon / lon_step).floor() * lon_step;
        while x_grid <= right_lon {
            let (px, _) = to_px(x_grid, lat0);
            let pxi = px as i32;
            if (0..w as i32).contains(&pxi) {
                let mut y = 0;
                while y < h as i32 {
                    img.put_pixel(pxi as u32, y as u32, GRID);
                    y += 1;
                }
            }
            x_grid += lon_step;
        }
        let mut y_grid = (bottom_lat / lat_step).floor() * lat_step;
        while y_grid <= top_lat {
            let (_, py) = to_px(lon0, y_grid);
            let pyi = py as i32;
            if (0..h as i32).contains(&pyi) {
                let mut xx = 0;
                while xx < w as i32 {
                    img.put_pixel(xx as u32, pyi as u32, GRID);
                    xx += 1;
                }
            }
            y_grid += lat_step;
        }

        // name -> (像素坐标, 类型)
        let mut pos: std::collections::HashMap<String, (f32, f32)> =
            std::collections::HashMap::new();
        for hb in hubs {
            pos.insert(hb.name.clone(), to_px(hb.lon, hb.lat));
        }
        for p in points {
            pos.insert(p.name.clone(), to_px(p.lon, p.lat));
        }

        // 3. 意会圈（虚线圆）：底层，先画
        let circle_px = |r_km: f64| -> f32 { (r_km * scale / 110.9) as f32 };
        for (h, r) in hubs.iter().zip(&radii) {
            let (cx, cy) = pos[&h.name];
            draw_dashed_circle(&mut img, cx, cy, circle_px(*r), MARKER_RING, 2);
        }

        // 4. 辐射细虚线：偏远点 → 区圈边（多归属都画），画在大交通下层
        for p in points {
            let d_to_hubs: Vec<f64> = hubs
                .iter()
                .map(|hb| dist_km(hb.lon, hb.lat, p.lon, p.lat))
                .collect();
            if d_to_hubs.iter().copied().fold(f64::INFINITY, f64::min) <= T_NEAR_KM {
                continue; // 区内点，不画辐射线
            }
            let ppx = pos[&p.name];
            for hi in remote_targets(p, hubs) {
                let (hx, hy) = pos[&hubs[hi].name];
                let r_px = circle_px(radii[hi]);
                let dx = ppx.0 - hx;
                let dy = ppx.1 - hy;
                let len = (dx * dx + dy * dy).sqrt().max(1.0);
                // 终点缩到圈边：hub 中心 + 单位向量 × 圈半径
                let rim = (hx + dx / len * r_px, hy + dy / len * r_px);
                draw_thick_line(
                    &mut img,
                    rim,
                    ppx,
                    &LineStyle {
                        color: REMOTE_LINE,
                        width: 3,
                        dash: true,
                    },
                );
            }
        }

        // 5. 大交通路线（同起终点重合路线沿垂直方向偏移错开）
        use std::collections::HashMap as StdMap;
        let mut pair_total: StdMap<(String, String), usize> = StdMap::new();
        for r in routes.iter() {
            let pair = if r.from <= r.to {
                (r.from.clone(), r.to.clone())
            } else {
                (r.to.clone(), r.from.clone())
            };
            *pair_total.entry(pair).or_insert(0) += 1;
        }
        let mut pair_counter: StdMap<(String, String), i32> = StdMap::new();
        for (i, r) in routes.iter().enumerate() {
            let pair = if r.from <= r.to {
                (r.from.clone(), r.to.clone())
            } else {
                (r.to.clone(), r.from.clone())
            };
            let total = *pair_total.get(&pair).unwrap_or(&1) as i32;
            let step = 20.0_f32;
            let count = pair_counter.entry(pair.clone()).or_insert(0);
            let offset_idx = *count;
            *count += 1;
            let offset_px = if total <= 1 {
                0.0_f32
            } else {
                (offset_idx as f32 - (total - 1) as f32 / 2.0) * step
            };
            let a = pos.get(&pair.0).copied().unwrap_or((0.0, 0.0));
            let b = pos.get(&pair.1).copied().unwrap_or((0.0, 0.0));
            let dx = b.0 - a.0;
            let dy = b.1 - a.1;
            let len = (dx * dx + dy * dy).sqrt().max(1.0);
            let ox = -dy / len * offset_px;
            let oy = dx / len * offset_px;
            let a_off = (a.0 + ox, a.1 + oy);
            let b_off = (b.0 + ox, b.1 + oy);
            let style = LineStyle {
                color: PALETTE[i % PALETTE.len()],
                width: 7,
                dash: true,
            };
            draw_thick_line(&mut img, a_off, b_off, &style);
        }

        // 6. 标记点（hub 大环 + 点位三样式），盖在线上
        for hb in hubs {
            let (x, y) = pos[&hb.name];
            let (cx, cy) = (x as i32, y as i32);
            draw_filled_circle_mut(&mut img, (cx, cy), 12, HALO);
            draw_hollow_circle_mut(&mut img, (cx, cy), 10, MARKER_RING);
            draw_filled_circle_mut(&mut img, (cx, cy), 3, MARKER_CORE);
        }
        for p in points {
            let (x, y) = pos[&p.name];
            draw_point_marker(&mut img, parse_point_type(p.point_type.as_deref()), x, y);
        }

        // 6.5 名称 + 交通方式 + 经纬度标注 + 指北针（字体加载失败则只跳过文字）
        if let Some(ref f) = font {
            let scale_hub = ab_glyph::PxScale::from(34.0);
            let scale_pt = ab_glyph::PxScale::from(26.0);
            // 标签：单点画全名；重叠打包组只画概述（成员明细在图例），marker 已逐个画过
            for &i in &singles {
                let (is_hub, name, lon, lat) = &entries[i];
                let (x, y) = project(*lon, *lat, cy_img);
                draw_label(
                    &mut img,
                    f,
                    if *is_hub { scale_hub } else { scale_pt },
                    name,
                    x,
                    y,
                    w,
                );
            }
            for (summary, anchor) in &packed_summaries {
                let (is_hub, _, lon, lat) = &entries[*anchor];
                let (x, y) = project(*lon, *lat, cy_img);
                draw_label(
                    &mut img,
                    f,
                    if *is_hub { scale_hub } else { scale_pt },
                    summary,
                    x,
                    y,
                    w,
                );
            }
            // 交通方式：每段路线中点（加偏移），用该段颜色标注
            let scale_sm = ab_glyph::PxScale::from(26.0);
            let mut pair_idx2: StdMap<(String, String), usize> = StdMap::new();
            for (i, r) in routes.iter().enumerate() {
                let Some(t) = r.transport.as_deref() else {
                    continue;
                };
                let pair = if r.from <= r.to {
                    (r.from.clone(), r.to.clone())
                } else {
                    (r.to.clone(), r.from.clone())
                };
                let total = *pair_total.get(&pair).unwrap_or(&1) as i32;
                let step = 40.0_f32;
                let count = pair_idx2.entry(pair.clone()).or_insert(0);
                let offset_idx = *count;
                *count += 1;
                let offset_px = if total <= 1 {
                    0.0_f32
                } else {
                    (offset_idx as f32 - (total - 1) as f32 / 2.0) * step
                };
                let a = pos.get(&pair.0).copied().unwrap_or((0.0, 0.0));
                let b = pos.get(&pair.1).copied().unwrap_or((0.0, 0.0));
                let dx = b.0 - a.0;
                let dy = b.1 - a.1;
                let len = (dx * dx + dy * dy).sqrt().max(1.0);
                let ox = dx / len * offset_px;
                let oy = dy / len * offset_px;
                let mid = ((a.0 + b.0) / 2.0 + ox, (a.1 + b.1) / 2.0 + oy);
                let color = PALETTE[i % PALETTE.len()];
                for (dx, dy) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
                    draw_text_mut(
                        &mut img,
                        HALO,
                        mid.0 as i32 + dx,
                        mid.1 as i32 + dy,
                        scale_sm,
                        f,
                        t,
                    );
                }
                draw_text_mut(&mut img, color, mid.0 as i32, mid.1 as i32, scale_sm, f, t);
            }
            // 经纬度标注：经度标在上边、纬度标在左边，淡灰蓝
            let scale_ll = ab_glyph::PxScale::from(18.0);
            let mut xg = (left_lon / lon_step).floor() * lon_step;
            while xg <= right_lon {
                let (px, _) = to_px(xg, lat0);
                let pxi = px as i32;
                if (0..w as i32).contains(&pxi) {
                    let label = format!("{xg:.lon_decimals$}°E");
                    draw_text_mut(&mut img, LONLAT_LABEL, pxi + 2, 2, scale_ll, f, &label);
                }
                xg += lon_step;
            }
            let mut yg = (bottom_lat / lat_step).floor() * lat_step;
            while yg <= top_lat {
                let (_, py) = to_px(lon0, yg);
                let pyi = py as i32;
                if (0..h as i32).contains(&pyi) {
                    let label = format!("{yg:.lat_decimals$}°N");
                    draw_text_mut(&mut img, LONLAT_LABEL, 2, pyi + 2, scale_ll, f, &label);
                }
                yg += lat_step;
            }
            // 指北针 N↑（右上角）
            let ncx = w as i32 - 45;
            let ntip = 30;
            let nbase = ntip + 44;
            let shaft = LineStyle {
                color: MARKER_RING,
                width: 4,
                dash: false,
            };
            draw_thick_line(
                &mut img,
                (ncx as f32, ntip as f32),
                (ncx as f32, nbase as f32),
                &shaft,
            );
            draw_thick_line(
                &mut img,
                (ncx as f32, ntip as f32),
                (ncx as f32 - 8.0, (ntip + 14) as f32),
                &shaft,
            );
            draw_thick_line(
                &mut img,
                (ncx as f32, ntip as f32),
                (ncx as f32 + 8.0, (ntip + 14) as f32),
                &shaft,
            );
            let scale_n = ab_glyph::PxScale::from(22.0);
            draw_text_mut(&mut img, MARKER_RING, ncx - 7, ntip - 22, scale_n, f, "N");

            // 图例框（底部通栏）
            draw_flow_legend(&mut img, &items, f);
        }

        Ok((img, packed_groups))
    }
}

/// 标签矩形（与 draw_label 摆放同源）：右下偏移；贴近右缘翻转到标记左侧。
/// 返回 (tx, ty, 文字宽, 文字高)。重叠打包检测与绘制共用，勿只改一处。
fn label_rect(
    scale: ab_glyph::PxScale,
    name: &str,
    x: f32,
    y: f32,
    img_w: u32,
    font: &ab_glyph::FontVec,
) -> (f32, f32, f32, f32) {
    let (tw, th) = text_size(scale, font, name);
    let (tw, th) = (tw as f32, th as f32);
    let tx = if x as i32 + 14 + tw as i32 > img_w as i32 - 4 {
        x - 14.0 - tw
    } else {
        x + 14.0
    };
    (tx, y + 8.0, tw, th)
}

/// 画名称标签：位置按 label_rect；白光晕描边。
fn draw_label(
    img: &mut RgbaImage,
    font: &ab_glyph::FontVec,
    scale: ab_glyph::PxScale,
    name: &str,
    x: f32,
    y: f32,
    img_w: u32,
) {
    let (tx, ty, _, _) = label_rect(scale, name, x, y, img_w, font);
    let (tx, ty) = (tx as i32, ty as i32);
    for (dx, dy) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
        draw_text_mut(img, HALO, tx + dx, ty + dy, scale, font, name);
    }
    draw_text_mut(img, MARKER_RING, tx, ty, scale, font, name);
}

/// 两标签矩形是否相交（各向外扩 PAD，视觉上贴得太近的也算重叠）。
fn rects_overlap(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> bool {
    const PAD: f32 = 4.0;
    a.0 - PAD < b.0 + b.2 + PAD
        && b.0 - PAD < a.0 + a.2 + PAD
        && a.1 - PAD < b.1 + b.3 + PAD
        && b.1 - PAD < a.1 + a.3 + PAD
}

/// 标签归组：矩形相交即同组，相交关系链式传递（复用单链接聚类，dist=相交?0:1）。
fn group_overlaps(rects: &[(f32, f32, f32, f32)]) -> Vec<Vec<usize>> {
    single_link_clusters(
        rects.len(),
        |i, j| {
            if rects_overlap(rects[i], rects[j]) {
                0.0
            } else {
                1.0
            }
        },
        0.5,
    )
}

/// 打包组 →（概述文本, 明细成员名, 锚点下标=组内最靠前者）。
/// 恰含 1 个 hub → 「{hub}（N 处）」，hub 名即概述不重复进明细；
/// 无 hub → 「{首点}一带（N 处）」；≥2 个 hub（罕见，如邻市同图）→ 「{首hub}等（N 处）」。
fn pack_summary(
    entries: &[(bool, String, f64, f64)],
    group: &[usize],
) -> (String, Vec<String>, usize) {
    let hub_pos: Vec<usize> = group.iter().filter(|&&i| entries[i].0).copied().collect();
    let anchor = group[0];
    if hub_pos.len() == 1 {
        let h = hub_pos[0];
        let names: Vec<String> = group
            .iter()
            .filter(|&&i| i != h)
            .map(|&i| entries[i].1.clone())
            .collect();
        (
            format!("{}（{} 处）", entries[h].1, names.len()),
            names,
            anchor,
        )
    } else if hub_pos.is_empty() {
        let names: Vec<String> = group.iter().map(|&i| entries[i].1.clone()).collect();
        (
            format!("{}一带（{} 处）", entries[anchor].1, names.len()),
            names,
            anchor,
        )
    } else {
        let names: Vec<String> = group
            .iter()
            .filter(|&&i| i != hub_pos[0])
            .map(|&i| entries[i].1.clone())
            .collect();
        (
            format!("{}等（{} 处）", entries[hub_pos[0]].1, group.len()),
            names,
            anchor,
        )
    }
}

/// 点位标记样式（带白光晕）。
fn draw_point_marker(img: &mut RgbaImage, kind: PointKind, x: f32, y: f32) {
    let (cx, cy) = (x as i32, y as i32);
    match kind {
        PointKind::Station => {
            draw_filled_rect_mut(img, Rect::at(cx - 8, cy - 8).of_size(16, 16), HALO);
            draw_filled_rect_mut(img, Rect::at(cx - 5, cy - 5).of_size(10, 10), MARKER_RING);
        }
        PointKind::Airport => {
            let wing = 7.0_f32;
            for pass in 0..2 {
                let (color, width) = if pass == 0 {
                    (HALO, 8)
                } else {
                    (MARKER_RING, 4)
                };
                let st = LineStyle {
                    color,
                    width,
                    dash: false,
                };
                draw_thick_line(img, (x - wing, y - wing), (x + wing, y + wing), &st);
                draw_thick_line(img, (x - wing, y + wing), (x + wing, y - wing), &st);
            }
        }
        PointKind::Scenic => {
            draw_filled_circle_mut(img, (cx, cy), 7, HALO);
            draw_filled_circle_mut(img, (cx, cy), 4, MARKER_RING);
        }
        PointKind::City => {
            draw_filled_circle_mut(img, (cx, cy), 11, HALO);
            draw_hollow_circle_mut(img, (cx, cy), 9, MARKER_RING);
            draw_filled_circle_mut(img, (cx, cy), 2, MARKER_CORE);
        }
    }
}

/// 虚线圆：按角度步进画短弧段（弧长 ≈6px 画、5px 跳）。
fn draw_dashed_circle(img: &mut RgbaImage, cx: f32, cy: f32, r: f32, color: Rgba<u8>, width: i32) {
    if r <= 2.0 {
        return;
    }
    let arc_px = 6.0_f32;
    let cycle_px = 11.0_f32; // 画 6 + 跳 5
    let style = LineStyle {
        color,
        width,
        dash: false,
    };
    let tau = std::f32::consts::TAU;
    let mut theta = 0.0_f32;
    while theta < tau {
        let end = (theta + arc_px / r).min(tau);
        let a = (cx + r * theta.cos(), cy + r * theta.sin());
        let b = (cx + r * end.cos(), cy + r * end.sin());
        draw_thick_line(img, a, b, &style);
        theta += cycle_px / r;
    }
}

/// 按像素间距选「好看的」整数度档（10^n × {1,2,5}，向上取）：
/// raw = target_px / px_per_deg，档位使实际像素间距落在 [target, ~2.5×target)。
/// 网格线密度由像素口径统一，经纬线观感一致；标注仍是整数度友好值。
fn nice_step_for_px(target_px: f64, px_per_deg: f64) -> f64 {
    let raw = (target_px / px_per_deg).max(0.0001);
    let exp = raw.log10().floor();
    let base = 10f64.powi(exp as i32);
    let n = raw / base;
    let nice = if n <= 1.0 {
        1.0
    } else if n <= 2.0 {
        2.0
    } else if n <= 5.0 {
        5.0
    } else {
        10.0
    };
    nice * base
}

#[cfg(test)]
mod tests {
    use super::super::tests::test_font_path;
    use super::*;

    /// 陕北 part 场景实跑：延安/榆林两个交通便利区 + 区内点/辐射景点（波浪谷多归属）+
    /// 火车站/机场标记 + 1 条大交通。肉眼检查意会圈/辐射线/三样式标记。
    /// 运行：`cargo test overview_part_real -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn overview_part_real() {
        let tool = GenerateMap::new(test_font_path(), "test".into());
        let args = serde_json::json!({
            "hubs": [
                {"name": "延安市区", "lon": 109.49, "lat": 36.60},
                {"name": "榆林市区", "lon": 109.75, "lat": 38.29}
            ],
            "points": [
                {"name": "宝塔山", "lon": 109.492, "lat": 36.617, "type": "scenic"},
                {"name": "延安站", "lon": 109.51, "lat": 36.61, "type": "station"},
                {"name": "壶口瀑布", "lon": 110.47, "lat": 36.13, "type": "scenic"},
                {"name": "波浪谷", "lon": 108.85, "lat": 37.58, "type": "scenic",
                 "hubs": ["延安市区", "榆林市区"]},
                {"name": "红碱淖", "lon": 109.9, "lat": 39.05, "type": "scenic"},
                {"name": "榆林榆阳机场", "lon": 109.74, "lat": 38.17, "type": "airport"}
            ],
            "routes": [
                {"from": "延安市区", "to": "榆林市区", "transport": "大巴"}
            ]
        });
        let out = tool.execute(args).await.expect("execute 失败");
        println!("\n=== generate_overview_map（part 化）输出 ===\n{out}\n");
        assert!(out.contains("已生成总览图"), "输出异常: {out}");
    }

    /// 阶段1 位置关系图实跑：hubs 留空、只列候选点位（type=city），不画圈不画交通。
    /// 运行：`cargo test overview_points_only -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn overview_points_only() {
        let tool = GenerateMap::new(test_font_path(), "test".into());
        let args = serde_json::json!({
            "hubs": [],
            "points": [
                {"name": "延安", "lon": 109.49, "lat": 36.60, "type": "city"},
                {"name": "榆林", "lon": 109.75, "lat": 38.29, "type": "city"},
                {"name": "壶口瀑布", "lon": 110.47, "lat": 36.13, "type": "scenic"},
                {"name": "波浪谷", "lon": 108.85, "lat": 37.58, "type": "scenic"}
            ],
            "routes": []
        });
        let out = tool.execute(args).await.expect("execute 失败");
        println!("\n=== generate_overview_map（位置关系图）输出 ===\n{out}\n");
        assert!(out.contains("已生成位置关系图"), "输出异常: {out}");
    }

    /// 纯函数：意会圈半径 = 区内点最远距离×1.2；偏远点不参与；无区内点下限 4km。
    #[test]
    fn hub_radii_rules() {
        let hubs = vec![Hub {
            name: "A".into(),
            lon: 109.0,
            lat: 36.0,
        }];
        // 区内点 10km / 15km，偏远点 100km
        let far = (109.0 + 100.0 / (111.32 * 36.0_f64.to_radians().cos()), 36.0);
        let points = vec![
            point_at("近1", 109.0, 36.0 + 10.0 / 110.574),
            point_at("近2", 109.0, 36.0 + 15.0 / 110.574),
            point_at("远", far.0, far.1),
        ];
        let radii = hub_radii(&hubs, &points);
        assert!(
            (radii[0] - 18.0).abs() < 0.5,
            "半径应为 15×1.2=18km，实际 {}",
            radii[0]
        );
        // 无区内点 → 下限 4km
        let radii2 = hub_radii(&hubs, &[point_at("远", far.0, far.1)]);
        assert!((radii2[0] - 4.0).abs() < 1e-9, "无区内点半径应为下限 4km");
    }

    /// 纯函数：偏远点辐射目标——自动多归属（d ≤ 2×dmin）与显式 hubs 覆盖。
    #[test]
    fn remote_targets_rules() {
        let hubs = vec![
            Hub {
                name: "延安".into(),
                lon: 109.49,
                lat: 36.60,
            },
            Hub {
                name: "榆林".into(),
                lon: 109.75,
                lat: 38.29,
            },
        ];
        // 波浪谷：延安/榆林都在 2×dmin 内 → 多归属
        let blg = point_at("波浪谷", 108.85, 37.58);
        let d = |i: usize| dist_km(hubs[i].lon, hubs[i].lat, blg.lon, blg.lat);
        let targets = remote_targets(&blg, &hubs);
        let dmin = d(0).min(d(1));
        assert_eq!(
            targets.len(),
            2,
            "波浪谷应多归属（{}km vs {}km）",
            d(0),
            d(1)
        );
        assert!(d(0) <= 2.0 * dmin + 1e-9 && d(1) <= 2.0 * dmin + 1e-9);
        // 壶口瀑布：榆林明显更远 → 单归属延安（最近）
        let hk = point_at("壶口瀑布", 110.47, 36.13);
        let targets2 = remote_targets(&hk, &hubs);
        assert_eq!(targets2, vec![0], "壶口应只挂靠最近的延安");
        // 显式 hubs 覆盖：指定榆林 → 只榆林
        let mut explicit = point_at("某点", 109.8, 37.5);
        explicit.hubs = Some(vec!["榆林".into()]);
        assert_eq!(remote_targets(&explicit, &hubs), vec![1]);
        // 空 hubs 数组视为未传 → 自动挂靠（该点到两 hub 都在 2×dmin 内）
        explicit.hubs = Some(Vec::new());
        assert_eq!(remote_targets(&explicit, &hubs).len(), 2);
    }

    /// 类型别名解析：英文/中文/缺省。
    #[test]
    fn parse_point_type_aliases() {
        assert_eq!(parse_point_type(Some("station")), PointKind::Station);
        assert_eq!(parse_point_type(Some("高铁站")), PointKind::Station);
        assert_eq!(parse_point_type(Some("airport")), PointKind::Airport);
        assert_eq!(parse_point_type(Some("机场")), PointKind::Airport);
        assert_eq!(parse_point_type(Some("出发地")), PointKind::City);
        assert_eq!(parse_point_type(Some("scenic")), PointKind::Scenic);
        assert_eq!(parse_point_type(None), PointKind::Scenic);
        assert_eq!(parse_point_type(Some("随便写的")), PointKind::Scenic);
    }

    /// 网格步长选档：经纬线像素间距由同一目标统一，比值受 1-2-5 量化上界约束（<2.5×）；
    /// 标注小数位随步长走。
    #[test]
    fn grid_steps_pixel_consistent() {
        // 郑州场景实测投影参数：scale=2374 px/°lat，cos=0.821，内容 937×640 px
        let (px_lon, px_lat) = (0.821 * 2374.0, 2374.0);
        let target = (937.0_f64.max(640.0) / 6.0).clamp(90.0, 260.0);
        let lon_step = nice_step_for_px(target, px_lon);
        let lat_step = nice_step_for_px(target, px_lat);
        assert!(
            (lon_step - 0.1).abs() < 1e-9,
            "lon step 应为 0.1°，实际 {lon_step}"
        );
        assert!(
            (lat_step - 0.1).abs() < 1e-9,
            "lat step 应为 0.1°，实际 {lat_step}"
        );
        // 两轴像素间距比值必须 < 2.5（1-2-5 向上取档的最坏跳档比）
        let ratio = (lon_step * px_lon) / (lat_step * px_lat);
        assert!(
            (0.4..2.5).contains(&ratio),
            "经纬线像素间距失配: ratio={ratio}"
        );
        // 高纬极端：cos=0.5，两轴同度数档 → 经线像素间距恰为纬线一半，仍在界内
        let (px_lon2, px_lat2) = (500.0, 1000.0);
        let s_lon = nice_step_for_px(150.0, px_lon2);
        let s_lat = nice_step_for_px(150.0, px_lat2);
        let ratio2 = (s_lon * px_lon2) / (s_lat * px_lat2);
        assert!((0.4..2.5).contains(&ratio2), "高纬失配: ratio={ratio2}");
        // 小数位规则
        assert!(nice_step_for_px(260.0, 100.0) >= 1.0);
        assert!(nice_step_for_px(9.0, 100.0) < 0.1 + 1e-9);
    }

    /// 标签重叠归组：相交合并、链式传递（A叠B、B叠C → 一组）、不相交独立。
    #[test]
    fn label_overlap_grouping() {
        let rects = vec![
            (0.0, 0.0, 100.0, 30.0),     // 0
            (90.0, 10.0, 100.0, 30.0),   // 1 与 0 相交
            (500.0, 500.0, 100.0, 30.0), // 2 独立
            (30.0, 25.0, 80.0, 30.0),    // 3 与 0、1 都相交 → 链式同组
        ];
        assert_eq!(group_overlaps(&rects), vec![vec![0, 1, 3], vec![2]]);
        // 全不相交 → 各自成组
        let far = vec![(0.0, 0.0, 50.0, 20.0), (200.0, 0.0, 50.0, 20.0)];
        assert_eq!(group_overlaps(&far), vec![vec![0], vec![1]]);
    }

    /// 打包概述三形态：1 hub / 无 hub / 2 hubs。
    #[test]
    fn pack_summary_forms() {
        let entries = vec![
            (true, "郑州市区".to_string(), 113.6, 34.7),
            (false, "二七广场".to_string(), 113.6, 34.7),
            (false, "郑州火车站".to_string(), 113.6, 34.7),
        ];
        let (s, names, anchor) = pack_summary(&entries, &[0, 1, 2]);
        assert_eq!(s, "郑州市区（2 处）");
        assert_eq!(names, vec!["二七广场", "郑州火车站"]);
        assert_eq!(anchor, 0);
        // 无 hub：{首点}一带
        let e2 = vec![
            (false, "只有河南戏剧幻城".to_string(), 0.0, 0.0),
            (false, "电影小镇".to_string(), 0.0, 0.0),
        ];
        let (s2, n2, a2) = pack_summary(&e2, &[0, 1]);
        assert_eq!(s2, "只有河南戏剧幻城一带（2 处）");
        assert_eq!(n2.len(), 2);
        assert_eq!(a2, 0);
        // 2 hubs（罕见）：{首hub}等
        let e3 = vec![
            (true, "郑州".to_string(), 0.0, 0.0),
            (true, "开封".to_string(), 0.0, 0.0),
            (false, "景点".to_string(), 0.0, 0.0),
        ];
        let (s3, n3, _) = pack_summary(&e3, &[0, 1, 2]);
        assert_eq!(s3, "郑州等（3 处）");
        assert_eq!(n3, vec!["开封", "景点"]);
    }

    fn point_at(name: &str, lon: f64, lat: f64) -> PointNode {
        PointNode {
            name: name.into(),
            lon,
            lat,
            point_type: None,
            hubs: None,
        }
    }
}
