//! 地图工具模块：overview（总览图）+ city（城市详细图）+ 共用绘图工具。
//! 对外导出两个 Tool：[`GenerateMap`]（generate_overview_map）与 [`GenerateCityMap`]（generate_city_map）。
//! 图片产物存 maps/{session_id}/，会话删除联动清理。

mod city;
mod overview;

pub use city::GenerateCityMap;
pub use overview::GenerateMap;

use image::{Rgba, RgbaImage};
use imageproc::drawing::{
    draw_filled_circle_mut, draw_filled_rect_mut, draw_hollow_circle_mut, draw_hollow_rect_mut,
    draw_text_mut, text_size,
};
use imageproc::rect::Rect;

use super::Tool;

/// LLM 传入的路线段（overview/city 共用；transport 在 city 里作交通方式配色用）。
#[derive(Deserialize)]
pub(crate) struct RouteSeg {
    pub(crate) from: String,
    pub(crate) to: String,
    #[serde(default)]
    pub(crate) transport: Option<String>,
}

/// 路线样式：dash=true 时彩色+淡灰交替（条纹）。
pub(crate) struct LineStyle {
    pub(crate) color: Rgba<u8>,
    pub(crate) width: i32, // 像素，用圆形戳子半径 = width/2 模拟粗线
    pub(crate) dash: bool,
}

/// 标记/文字深色（两图共用）。
pub(crate) const MARKER_RING: Rgba<u8> = Rgba([30, 41, 59, 255]);

use serde::Deserialize;

/// 懒加载 CJK 字体。路径为空或加载失败返回 None（不画文字标签/图例）。
pub(crate) fn load_font(font_path: &str) -> Option<ab_glyph::FontVec> {
    if font_path.is_empty() {
        return None;
    }
    let bytes = std::fs::read(font_path).ok()?;
    ab_glyph::FontVec::try_from_vec(bytes).ok()
}

/// 半透明叠加：over 画到 bg 上（按 over 的 alpha 混合）。
pub(crate) fn blend_pixel(over: Rgba<u8>, bg: Rgba<u8>) -> Rgba<u8> {
    let a = over[3] as f32 / 255.0;
    Rgba([
        (over[0] as f32 * a + bg[0] as f32 * (1.0 - a)) as u8,
        (over[1] as f32 * a + bg[1] as f32 * (1.0 - a)) as u8,
        (over[2] as f32 * a + bg[2] as f32 * (1.0 - a)) as u8,
        255,
    ])
}

/// 画粗线：沿线段戳圆形（半径 = width/2）。dash=true 时彩色段+淡灰段交替（条纹）。
pub(crate) fn draw_thick_line(
    img: &mut RgbaImage,
    a: (f32, f32),
    b: (f32, f32),
    style: &LineStyle,
) {
    let radius = (style.width as f32 / 2.0).max(1.0).ceil() as i32;
    let dx = b.0 - a.0;
    let dy = b.1 - a.1;
    let dist = (dx * dx + dy * dy).sqrt();
    let steps = dist.ceil() as i32;
    // 条纹参数：彩色段 14px + 间隔 8px
    const DASH_ON: f32 = 14.0;
    const DASH_OFF: f32 = 8.0;
    const DASH_CYCLE: f32 = DASH_ON + DASH_OFF;
    for i in 0..=steps {
        let t = if steps == 0 {
            0.0
        } else {
            i as f32 / steps as f32
        };
        let x = a.0 + dx * t;
        let y = a.1 + dy * t;
        let color = if style.dash {
            let walked = i as f32;
            let phase = walked % DASH_CYCLE;
            if phase < DASH_ON {
                style.color
            } else {
                DASH_GAP
            }
        } else {
            style.color
        };
        imageproc::drawing::draw_filled_circle_mut(img, (x as i32, y as i32), radius, color);
    }
}

/// 条纹路线的间隔色（淡灰，与纯白底色拉开层次）。
pub(crate) const DASH_GAP: Rgba<u8> = Rgba([235, 239, 245, 255]);

/// 图例条目：按内容流式排布。
#[derive(Clone)]
pub(crate) enum LegendItem {
    /// 纯文本（标题 / 编号→名称）
    Text(String),
    /// 色块线 + 文本（交通方式）
    Swatch { color: Rgba<u8>, text: String },
    /// 空心圆环 + 圆心点 + 文本（交通便利区）
    Marker(String),
    /// 实心圆点 + 文本（景点）
    Dot(String),
    /// 实心方块 + 文本（火车站）
    Square(String),
    /// 叉号 + 文本（机场）
    Cross(String),
    /// 细线 + 文本（辐射关系）
    Thin { color: Rgba<u8>, text: String },
}

impl LegendItem {
    fn text(&self) -> &str {
        match self {
            LegendItem::Text(t)
            | LegendItem::Swatch { text: t, .. }
            | LegendItem::Marker(t)
            | LegendItem::Dot(t)
            | LegendItem::Square(t)
            | LegendItem::Cross(t)
            | LegendItem::Thin { text: t, .. } => t,
        }
    }
    /// 有无符号前缀（色块/圆/方块/叉占位宽）。
    fn has_symbol(&self) -> bool {
        !matches!(self, LegendItem::Text(_))
    }
}

/// 图例绘制单元：条目按宽度拆分后的最小布局单位。
/// 超宽条目会被按字符断行拆成多个单元（首块带色块/圆点前缀，续行为纯文本顶格）。
struct LegendUnit {
    kind: LegendItem,
    width: i32,
    /// 绘制前强制换行（长条目的续行顶格起排，不与前面的条目同行）
    break_before: bool,
}

// ---- 流式图例布局常量（绘制与测高共用，勿只改一处） ----
const LEGEND_TEXT_PX: f32 = 18.0;
const LEGEND_MARGIN: i32 = 12; // 距画面左右/下边距
const LEGEND_PAD: i32 = 8; // 框内边距
const LEGEND_ROW_H: i32 = 28; // 行高
const LEGEND_GAP: i32 = 18; // 条目水平间距
const LEGEND_SYM_W: i32 = 28; // 色块/圆标记前缀宽

/// 把条目展开为绘制单元：宽度不超框内可用宽的条目原样一个单元；
/// 超宽条目按字符断行拆成多个单元，保证任何单元宽度都 ≤ 可用宽（至少 1 字符/单元防死循环）。
fn legend_units(items: &[LegendItem], font: &ab_glyph::FontVec, img_w: i32) -> Vec<LegendUnit> {
    let max_w = img_w - 2 * LEGEND_MARGIN - 2 * LEGEND_PAD;
    let scale = ab_glyph::PxScale::from(LEGEND_TEXT_PX);
    let tw = |s: &str| text_size(scale, font, s).0 as i32;
    let mut units: Vec<LegendUnit> = Vec::new();
    for it in items {
        let text = it.text();
        let sym = if it.has_symbol() { LEGEND_SYM_W } else { 0 };
        let full = sym + tw(text);
        if full <= max_w || max_w <= sym {
            // 正常条目；或画布过窄容不下前缀——原样放一个单元（绘制端不会比这更差）
            units.push(LegendUnit {
                kind: it.clone(),
                width: full,
                break_before: false,
            });
            continue;
        }
        // 超宽：按字符断行（首块带色块/圆点前缀，续行顶格纯文本）
        let color = match it {
            LegendItem::Swatch { color, .. } | LegendItem::Thin { color, .. } => Some(*color),
            _ => None,
        };
        let chars: Vec<char> = text.chars().collect();
        let mut pos = 0usize;
        let mut first = true;
        while pos < chars.len() {
            let limit = if first { max_w - sym } else { max_w };
            let mut chunk = String::new();
            while pos < chars.len() {
                let mut cand = chunk.clone();
                cand.push(chars[pos]);
                if sym + tw(&cand) > limit && !chunk.is_empty() {
                    break;
                }
                chunk = cand;
                pos += 1;
            }
            if chunk.is_empty() {
                // 单字符即超宽：仍放一个，防死循环
                chunk.push(chars[pos]);
                pos += 1;
            }
            let (kind, width) = if first {
                (
                    match color {
                        Some(c) => match it {
                            LegendItem::Thin { .. } => LegendItem::Thin {
                                color: c,
                                text: chunk.clone(),
                            },
                            _ => LegendItem::Swatch {
                                color: c,
                                text: chunk.clone(),
                            },
                        },
                        None => match it {
                            LegendItem::Marker(_) => LegendItem::Marker(chunk.clone()),
                            LegendItem::Dot(_) => LegendItem::Dot(chunk.clone()),
                            LegendItem::Square(_) => LegendItem::Square(chunk.clone()),
                            LegendItem::Cross(_) => LegendItem::Cross(chunk.clone()),
                            _ => LegendItem::Text(chunk.clone()),
                        },
                    },
                    sym + tw(&chunk),
                )
            } else {
                (LegendItem::Text(chunk.clone()), tw(&chunk))
            };
            units.push(LegendUnit {
                kind,
                width,
                break_before: !first,
            });
            first = false;
        }
    }
    units
}

/// 贪心折行：把绘制单元下标打包成行（测高与绘制共用此布局）。
/// `break_before` 的单元强制另起一行（但不在空行上额外断）。
fn legend_rows(units: &[LegendUnit], max_w: i32) -> Vec<Vec<usize>> {
    let mut rows: Vec<Vec<usize>> = vec![Vec::new()];
    let mut cur_w = 0_i32;
    for (i, u) in units.iter().enumerate() {
        let row = rows.last_mut().unwrap();
        let add = if row.is_empty() {
            u.width
        } else {
            u.width + LEGEND_GAP
        };
        if !row.is_empty() && (u.break_before || cur_w + add > max_w) {
            rows.push(vec![i]);
            cur_w = u.width;
        } else {
            row.push(i);
            cur_w += add;
        }
    }
    rows
}

/// 流式图例框高；空条目/仅标题（≤1 条）不画框返回 0。
pub(crate) fn flow_legend_box_h(items: &[LegendItem], font: &ab_glyph::FontVec, img_w: i32) -> i32 {
    if items.len() <= 1 {
        return 0;
    }
    let units = legend_units(items, font, img_w);
    let max_w = img_w - 2 * LEGEND_MARGIN - 2 * LEGEND_PAD;
    let rows = legend_rows(&units, max_w);
    LEGEND_PAD * 2 + rows.len() as i32 * LEGEND_ROW_H
}

/// 图例在画面底部占用的总预留高度（框高 + 下边距）。plan_view / 总览图重心避让用。
pub(crate) fn flow_legend_band(items: &[LegendItem], font: &ab_glyph::FontVec, img_w: i32) -> i32 {
    flow_legend_box_h(items, font, img_w) + LEGEND_MARGIN
}

/// 底部通栏流式图例：条目从左到右按内容宽度排布，放不下自动折行；
/// 单条目超宽按字符断行（首行带色块/圆点前缀，续行顶格）。
/// 框高 = 行数×行高 + 内边距（无空白填充）。半透明白底 + 细边框。
pub(crate) fn draw_flow_legend(
    img: &mut RgbaImage,
    items: &[LegendItem],
    font: &ab_glyph::FontVec,
) {
    if items.len() <= 1 {
        return;
    }
    let (iw, ih) = (img.width() as i32, img.height() as i32);
    let scale = ab_glyph::PxScale::from(LEGEND_TEXT_PX);
    let units = legend_units(items, font, iw);
    let max_w = iw - 2 * LEGEND_MARGIN - 2 * LEGEND_PAD;
    let rows = legend_rows(&units, max_w);

    // 框：底部通栏
    let box_w = iw - 2 * LEGEND_MARGIN;
    let box_h = LEGEND_PAD * 2 + rows.len() as i32 * LEGEND_ROW_H;
    let bx = LEGEND_MARGIN;
    let by = ih - LEGEND_MARGIN - box_h;

    let bg = Rgba([255, 255, 255, 200]);
    for yy in by..by + box_h {
        for xx in bx..bx + box_w {
            let p = *img.get_pixel(xx as u32, yy as u32);
            img.put_pixel(xx as u32, yy as u32, blend_pixel(bg, p));
        }
    }
    draw_hollow_rect_mut(
        img,
        Rect::at(bx, by).of_size(box_w as u32, box_h as u32),
        MARKER_RING,
    );

    // 绘制：文本在行内垂直居中
    for (ri, row) in rows.iter().enumerate() {
        let mut x = bx + LEGEND_PAD;
        for &i in row {
            let text = units[i].kind.text();
            let th = text_size(scale, font, text).1 as i32;
            let ty = by + LEGEND_PAD + ri as i32 * LEGEND_ROW_H + (LEGEND_ROW_H - th).max(0) / 2;
            let cy = ty + th / 2;
            match &units[i].kind {
                LegendItem::Text(t) => draw_text_mut(img, MARKER_RING, x, ty, scale, font, t),
                LegendItem::Swatch { color, text } => {
                    draw_thick_line(
                        img,
                        (x as f32, cy as f32),
                        ((x + LEGEND_SYM_W - 6) as f32, cy as f32),
                        &LineStyle {
                            color: *color,
                            width: 6,
                            dash: false,
                        },
                    );
                    draw_text_mut(img, MARKER_RING, x + LEGEND_SYM_W, ty, scale, font, text);
                }
                LegendItem::Thin { color, text } => {
                    draw_thick_line(
                        img,
                        (x as f32, cy as f32),
                        ((x + LEGEND_SYM_W - 6) as f32, cy as f32),
                        &LineStyle {
                            color: *color,
                            width: 3,
                            dash: true,
                        },
                    );
                    draw_text_mut(img, MARKER_RING, x + LEGEND_SYM_W, ty, scale, font, text);
                }
                LegendItem::Marker(t) => {
                    draw_hollow_circle_mut(img, (x + 9, cy), 8, MARKER_RING);
                    draw_filled_circle_mut(img, (x + 9, cy), 2, MARKER_RING);
                    draw_text_mut(img, MARKER_RING, x + LEGEND_SYM_W, ty, scale, font, t);
                }
                LegendItem::Dot(t) => {
                    draw_filled_circle_mut(img, (x + 9, cy), 5, MARKER_RING);
                    draw_text_mut(img, MARKER_RING, x + LEGEND_SYM_W, ty, scale, font, t);
                }
                LegendItem::Square(t) => {
                    draw_filled_rect_mut(img, Rect::at(x + 4, cy - 5).of_size(10, 10), MARKER_RING);
                    draw_text_mut(img, MARKER_RING, x + LEGEND_SYM_W, ty, scale, font, t);
                }
                LegendItem::Cross(t) => {
                    let (x0, y0, x1, y1) = (x + 3, cy - 6, x + 15, cy + 6);
                    for (a, b, c, d) in [(x0, y0, x1, y1), (x0, y1, x1, y0)] {
                        draw_thick_line(
                            img,
                            (a as f32, b as f32),
                            (c as f32, d as f32),
                            &LineStyle {
                                color: MARKER_RING,
                                width: 4,
                                dash: false,
                            },
                        );
                    }
                    draw_text_mut(img, MARKER_RING, x + LEGEND_SYM_W, ty, scale, font, t);
                }
            }
            x += units[i].width + LEGEND_GAP;
        }
    }
}

/// 把 session_id 里的特殊字符替换成 _，避免路径越界。
pub(crate) fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// 扫描 maps/{session_id}/ 里已有的 {prefix}_{n}.png，返回最大编号 n；无目录/文件时返回 0。
/// 用于工具实例初始化计数器：重新进入旧会话时从最大编号续接，避免覆盖已生成的图。
/// 簇放大图 {prefix}_{n}_c{k}.png 中间含非数字字符，不会被计入。
pub(crate) fn scan_max_index(session_id: &str, prefix: &str) -> u32 {
    let dir = std::path::Path::new("maps").join(sanitize(session_id));
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let mut max = 0u32;
    for entry in entries.flatten() {
        let fname = entry.file_name();
        let Some(name) = fname.to_str() else {
            continue;
        };
        let Some(stem) = name
            .strip_prefix(prefix)
            .and_then(|r| r.strip_prefix('_'))
            .and_then(|r| r.strip_suffix(".png"))
        else {
            continue;
        };
        if let Ok(n) = stem.parse::<u32>() {
            max = max.max(n);
        }
    }
    max
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试辅助：读字体路径（先加载 .env，MAP_FONT_PATH 优先）。
    pub(crate) fn test_font_path() -> String {
        dotenvy::dotenv().ok();
        std::env::var("MAP_FONT_PATH").unwrap_or_default()
    }

    /// 测试辅助：读高德 key。
    pub(crate) fn test_amap_key() -> String {
        dotenvy::dotenv().ok();
        std::env::var("AMAP_API_KEY").expect("需设置 AMAP_API_KEY")
    }

    /// 超宽条目自动断行：所有单元不超可用宽，续行顶格（break_before），框高随之增高。
    /// 依赖本机字体（.env 的 MAP_FONT_PATH），缺失时静默跳过。
    #[test]
    fn flow_legend_wraps_overlong_items() {
        let font = match load_font(&test_font_path()) {
            Some(f) => f,
            None => return, // 无 CJK 字体环境跳过
        };
        let long = "北京动物园→北京植物园→香山公园→八大处公园 骑行";
        let items = vec![
            LegendItem::Text("图例".into()),
            LegendItem::Swatch {
                color: Rgba([29, 78, 216, 255]),
                text: long.into(),
            },
        ];
        // 窄画布强迫断行：可用宽 = 300 − 2×12 − 2×8 = 260
        let units = legend_units(&items, &font, 300);
        let max_w = 300 - 2 * LEGEND_MARGIN - 2 * LEGEND_PAD;
        for u in &units {
            assert!(u.width <= max_w, "单元超宽：{} > {max_w}", u.width);
        }
        assert!(units.len() > items.len(), "长条目应被拆成多个单元");
        // 首块带前缀（Swatch），续块为纯文本且强制换行
        assert!(matches!(units[1].kind, LegendItem::Swatch { .. }));
        for u in &units[2..] {
            assert!(u.break_before);
            assert!(matches!(u.kind, LegendItem::Text(_)));
        }
        // 行数 ≥ 2 → 框高大于单行框
        let box_h = flow_legend_box_h(&items, &font, 300);
        assert!(box_h >= LEGEND_PAD * 2 + 2 * LEGEND_ROW_H, "box_h={box_h}");
    }

    /// scan_max_index：取最大编号；簇图 city_2_c1.png 不计入；目录不存在返回 0。
    #[test]
    fn scan_max_index_basic() {
        let sid = "_ut_scan_max_index";
        let dir = std::path::Path::new("maps").join(sid);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            "overview_1.png",
            "overview_3.png",
            "city_2.png",
            "city_2_c1.png",
            "city_5_c2.png",
            "unrelated.png",
        ] {
            std::fs::write(dir.join(name), b"png").unwrap();
        }
        assert_eq!(scan_max_index(sid, "overview"), 3);
        assert_eq!(scan_max_index(sid, "city"), 2); // _c{k} 簇图不计入
        assert_eq!(scan_max_index(sid, "nope"), 0);
        assert_eq!(scan_max_index("_ut_no_such_dir", "overview"), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 探测字体能否渲染中文。运行：
    /// cargo test font_probe -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn font_probe() {
        use ab_glyph::{Font, FontVec};
        let path = test_font_path();
        println!("字体路径: {path}");
        if path.is_empty() {
            println!("font_path 为空");
            return;
        }
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                println!("读文件失败: {e}");
                return;
            }
        };
        println!("文件 {} 字节", bytes.len());
        match FontVec::try_from_vec(bytes) {
            Ok(f) => {
                println!("FontVec 加载成功");
                for c in "北京济南南京上海高铁地铁公交打车".chars() {
                    let id = f.glyph_id(c);
                    println!(
                        "  '{c}' glyph_id={:?} {}",
                        id,
                        if id.0 == 0 { "=缺失" } else { "" }
                    );
                }
            }
            Err(e) => println!("FontVec 加载失败: {e}"),
        }
    }

    /// 从 geocode 工具的文本输出解析经纬度（格式「…→ 经度 113.65，纬度 34.75（…）」）。
    fn parse_lonlat(tool_out: &str) -> (f64, f64) {
        let lon: f64 = tool_out
            .split("经度 ")
            .nth(1)
            .and_then(|s| s.split('，').next())
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or_else(|| panic!("geocode 输出缺经度: {tool_out}"));
        let lat: f64 = tool_out
            .split("纬度 ")
            .nth(1)
            .and_then(|s| s.split('（').next())
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or_else(|| panic!("geocode 输出缺纬度: {tool_out}"));
        (lon, lat)
    }

    /// 郑州端到端场景（真实数据全链路，对应 system.md 例 1）：
    /// geocode 7 个真实点位 + 郑州市区 anchor → cluster_pois 几何刻画 →
    /// route_check 打车口径抽查 → generate_overview_map（单 hub 意会圈+辐射线）→
    /// generate_city_map（地铁/打车路线 + 市区簇放大图）。
    /// 运行：cargo test zhengzhou_part_scenario -- --ignored --nocapture
    /// 产物在 maps/test/（`cargo run -- clean-test` 清理）。
    #[tokio::test]
    #[ignore]
    async fn zhengzhou_part_scenario() {
        let _ = dotenvy::dotenv();
        let key = test_amap_key();
        let font_path = test_font_path();
        let limiter = crate::tools::RateLimiter::new(400);

        // 1. geocode：7 个点位 + 郑州市区 anchor（共享限流，约 8×0.4s）
        let geo = crate::tools::Geocode::new(key.clone(), limiter.clone());
        let mut pts: Vec<(String, f64, f64)> = Vec::new();
        for name in [
            "二七广场",
            "河南博物院",
            "紫荆山公园",
            "郑州火车站",
            "只有河南戏剧幻城",
            "建业电影小镇",
            "黄河文化公园",
        ] {
            let out = geo
                .execute(serde_json::json!({"address": name, "city": "郑州"}))
                .await
                .expect("geocode execute 失败");
            println!("{out}");
            let (lon, lat) = parse_lonlat(&out);
            pts.push((name.to_string(), lon, lat));
        }
        let anchor_out = geo
            .execute(serde_json::json!({"address": "郑州", "city": "郑州"}))
            .await
            .expect("geocode execute 失败");
        println!("{anchor_out}");
        let (zz_lon, zz_lat) = parse_lonlat(&anchor_out);

        // 2. cluster_pois：几何刻画（预期：市区 4 点 + 黄河文化公园[27.6km≤30km]
        //    被 anchor 按点吸收；只有河南+电影小镇因相邻自成小簇，由 LLM 后判断定性为辐射景点）
        let out = crate::tools::ClusterPois
            .execute(serde_json::json!({
                "pois": pts.iter()
                    .map(|(n, lon, lat)| serde_json::json!({"name": n, "lon": lon, "lat": lat}))
                    .collect::<Vec<_>>(),
                "anchors": [{"name": "郑州市区", "lon": zz_lon, "lat": zz_lat}],
                "min_regions": 1
            }))
            .await
            .expect("cluster_pois execute 失败");
        println!("\n=== cluster_pois ===\n{out}");
        assert!(out.contains("郑州市区"), "cluster 输出缺 anchor 区:\n{out}");

        // 3. route_check：打车口径抽查（远辐射当天往返 + 区内打车方便度）
        let ll = |name: &str| -> (f64, f64) {
            pts.iter()
                .find(|(n, _, _)| n == name)
                .map(|(_, lon, lat)| (*lon, *lat))
                .unwrap_or_else(|| panic!("点位 {name} 不在坐标表中"))
        };
        let rc = crate::tools::RouteCheck::new(key.clone(), limiter.clone());
        for (from, to) in [
            ("二七广场", "只有河南戏剧幻城"),
            ("二七广场", "黄河文化公园"),
            ("紫荆山公园", "郑州火车站"),
        ] {
            let (olon, olat) = ll(from);
            let (dlon, dlat) = ll(to);
            let out = rc
                .execute(serde_json::json!({
                    "origin": {"name": from, "lon": olon, "lat": olat},
                    "destination": {"name": to, "lon": dlon, "lat": dlat}
                }))
                .await
                .expect("route_check execute 失败");
            println!("{out}");
            assert!(out.contains("驾车约"), "route_check 输出异常: {out}");
        }

        // 4. overview：单 hub 意会圈 + 辐射线（routes 留空，单 part 无大交通）
        let overview = GenerateMap::new(font_path.clone(), "test".into());
        let remote = |name: &str| {
            serde_json::json!({
                "name": name, "lon": ll(name).0, "lat": ll(name).1,
                "type": "scenic", "hubs": ["郑州市区"]
            })
        };
        let (slon, slat) = ll("郑州火车站");
        let out = overview
            .execute(serde_json::json!({
                "hubs": [{"name": "郑州市区", "lon": zz_lon, "lat": zz_lat}],
                "points": [
                    {"name": "二七广场", "lon": ll("二七广场").0, "lat": ll("二七广场").1},
                    {"name": "河南博物院", "lon": ll("河南博物院").0, "lat": ll("河南博物院").1},
                    {"name": "紫荆山公园", "lon": ll("紫荆山公园").0, "lat": ll("紫荆山公园").1},
                    {"name": "郑州火车站", "lon": slon, "lat": slat, "type": "station"},
                    remote("只有河南戏剧幻城"),
                    remote("建业电影小镇"),
                    remote("黄河文化公园")
                ],
                "routes": []
            }))
            .await
            .expect("overview execute 失败");
        println!("\n=== generate_overview_map ===\n{out}");
        assert!(out.contains("已生成总览图"), "overview 输出异常: {out}");

        // 5. city map：地铁+打车路线（触达 transit/driving 两路 API），市区 4 点成簇应追加放大图
        let city = GenerateCityMap::new(key.clone(), font_path, "test".into(), limiter);
        let pois_json: Vec<serde_json::Value> = pts
            .iter()
            .map(|(n, lon, lat)| serde_json::json!({"name": n, "lon": lon, "lat": lat}))
            .collect();
        let out = city
            .execute(serde_json::json!({
                "city": "郑州",
                "pois": pois_json,
                "routes": [
                    {"from": "二七广场", "to": "紫荆山公园", "transport": "地铁"},
                    {"from": "紫荆山公园", "to": "河南博物院", "transport": "打车"},
                    {"from": "郑州火车站", "to": "二七广场", "transport": "打车"}
                ]
            }))
            .await
            .expect("city execute 失败");
        println!("\n=== generate_city_map ===\n{out}");
        assert!(out.contains("已生成城市详细图"), "city 输出异常: {out}");
    }
}
