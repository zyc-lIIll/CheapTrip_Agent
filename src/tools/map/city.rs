//! 城市详细图工具（generate_city_map）：高德路径规划拿真实 polyline + 高德静态地图渲染。
//! 路线按交通方式固定配色；POI≥5 时用编号标记+图例；聚类后大簇追加放大图。
//! 主图与簇放大图共用 draw_city_map 渲染流程，仅 pois 范围不同。
//! 产物 maps/{session_id}/city_{n}.png（+ city_{n}_c{k}.png 簇图）。

use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result};
use async_trait::async_trait;
use image::Rgba;
use serde::Deserialize;
use serde_json::Value;

use super::Tool;
use super::{LegendItem, RouteSeg, draw_flow_legend, sanitize};
use crate::tools::{dist_km, single_link_clusters};

/// 高德静态地图 zoom 偏置（探针实测）：请求 zoom=z 时，实际渲染比例对应 Web Mercator
/// 瓦片 zoom = z+1（marker 像素坐标簇与 tile z+1 预测吻合、与 tile z 相差一倍）。
/// 选 zoom 与中心换算必须统一经此偏置，否则实际渲染比预期密一倍，POI 会顶边裁切。
const AMAP_ZOOM_BIAS: u8 = 1;

/// 用 Web Mercator 投影（staticmap crate，256px 瓦片模型）算最高 zoom 让包围盒装进 fit 尺寸。
/// 返回值是高德 zoom 参数；校验时按实际渲染比例（tile = zoom + AMAP_ZOOM_BIAS）计算。
fn compute_zoom_fit(
    min_lon: f64,
    max_lon: f64,
    min_lat: f64,
    max_lat: f64,
    fit_w: f64,
    fit_h: f64,
) -> u8 {
    for a in (1..=17u8).rev() {
        let t = a + AMAP_ZOOM_BIAS;
        let px_w =
            (staticmap::lon_to_x(max_lon, t) - staticmap::lon_to_x(min_lon, t)).abs() * 256.0;
        let px_h =
            (staticmap::lat_to_y(min_lat, t) - staticmap::lat_to_y(max_lat, t)).abs() * 256.0;
        if px_w <= fit_w && px_h <= fit_h {
            return a;
        }
    }
    1
}

/// 城市图尺度预检：内容要压到 zoom < 8（跨度 ~150km+）才能装下 → 已不是城市尺度，
/// 渲染出来 POI 挤成一团无信息量。提示改用 generate_overview_map 或缩小范围。
fn bbox_too_large(bbox: (f64, f64, f64, f64), legend_band: f64) -> bool {
    let (fit_w, fit_h) = free_rect_size(legend_band);
    compute_zoom_fit(bbox.0, bbox.1, bbox.2, bbox.3, fit_w, fit_h) < 8
}

/// POI + 全部路线 polyline 的并集包围盒（度）。路线绕行可能远超 POI 范围，
/// 缩放/避让必须按并集算，否则路线会被画面边缘或图例带裁掉。
fn union_bbox(pois: &[PoiNode], routes: &[RouteData]) -> (f64, f64, f64, f64) {
    let mut min_lon = pois.iter().map(|p| p.lon).fold(f64::INFINITY, f64::min);
    let mut max_lon = pois.iter().map(|p| p.lon).fold(f64::NEG_INFINITY, f64::max);
    let mut min_lat = pois.iter().map(|p| p.lat).fold(f64::INFINITY, f64::min);
    let mut max_lat = pois.iter().map(|p| p.lat).fold(f64::NEG_INFINITY, f64::max);
    for r in routes {
        for pt in r.polyline.split(';') {
            let mut it = pt.split(',');
            if let (Some(lo), Some(la)) = (it.next(), it.next())
                && let (Ok(lo), Ok(la)) = (lo.trim().parse::<f64>(), la.trim().parse::<f64>())
            {
                min_lon = min_lon.min(lo);
                max_lon = max_lon.max(lo);
                min_lat = min_lat.min(la);
                max_lat = max_lat.max(la);
            }
        }
    }
    (min_lon, max_lon, min_lat, max_lat)
}

/// 视图规划：给定「POI+路线」并集包围盒，决定静态地图的 zoom 与中心。
///
/// 并集中心放到「边缘安全区 − 底部图例带」自由矩形的中心，zoom 压到
/// 并集 + 标签外扩能装进自由矩形为止——内容几何上不进图例带/画面边缘，不靠固定偏移猜。
/// 返回 (zoom, 中心经度, 中心纬度)。
/// 内容允许落占的自由矩形尺寸（图例避让 + 边缘安全边距 + 外扩），plan_view 与溢出预检共用。
fn free_rect_size(legend_band: f64) -> (f64, f64) {
    const W: f64 = 800.0;
    const H: f64 = 600.0;
    const EDGE: f64 = 24.0; // 画面边缘安全边距
    const PAD: f64 = 40.0; // 标签/编号徽章/描边外扩
    const SAFETY: f64 = 0.9; // 高德实际比例与 256px 瓦片模型的残差安全系数（实测 ~10% 内）

    let band = legend_band.clamp(0.0, 240.0);
    let (x0, y0, x1, y1) = (EDGE, EDGE, W - EDGE, H - band - EDGE);
    let fit_w = (x1 - x0) * SAFETY - 2.0 * PAD;
    let fit_h = (y1 - y0) * SAFETY - 2.0 * PAD;
    (fit_w, fit_h)
}

fn plan_view(bbox: (f64, f64, f64, f64), legend_band: f64) -> (u8, f64, f64) {
    const W: f64 = 800.0;
    const H: f64 = 600.0;
    const EDGE: f64 = 24.0; // 画面边缘安全边距

    // 内容允许落占的区域（图例避让 + 边缘安全边距）
    let (fit_w, fit_h) = free_rect_size(legend_band);
    let cx_px = (EDGE + (W - EDGE)) / 2.0;
    let cy_px = (EDGE + (H - legend_band.clamp(0.0, 240.0) - EDGE)) / 2.0;

    let (min_lon, max_lon, min_lat, max_lat) = bbox;
    let zoom = compute_zoom_fit(min_lon, max_lon, min_lat, max_lat, fit_w, fit_h);

    // 并集中心 → 放到目标像素位；换算按实际渲染比例（tile = zoom + AMAP_ZOOM_BIAS）。
    // 注意：lon_to_x 返回瓦片单位（1 tile = 256px），像素偏移换算除以 256；
    // location 要向目标位的反方向偏（loc 在西/北 → 内容出现在东/南）。
    let t = zoom + AMAP_ZOOM_BIAS;
    let c_lon = (min_lon + max_lon) / 2.0;
    let c_lat = (min_lat + max_lat) / 2.0;
    let dx_px = cx_px - W / 2.0;
    let dy_px = cy_px - H / 2.0;
    let lon = staticmap::x_to_lon(staticmap::lon_to_x(c_lon, t) - dx_px / 256.0, t);
    let lat = staticmap::y_to_lat(staticmap::lat_to_y(c_lat, t) - dy_px / 256.0, t);
    (zoom, lon, lat)
}

/// 后处理城市图：底部通栏半透明图例。
/// 图例条目 items 由 draw_city_map 构建传入（与测高/预留同源）。
fn compose_city_image(
    png_bytes: &[u8],
    factor: f32,
    items: &[LegendItem],
    font: Option<&ab_glyph::FontVec>,
) -> Result<Vec<u8>> {
    let orig = image::load_from_memory(png_bytes).context("解码 PNG 失败")?;

    // 淡化主图
    let mut main = orig.to_rgba8();
    let f = factor.clamp(0.0, 1.0);
    if f > 0.0 {
        for px in main.pixels_mut() {
            let Rgba([r, g, b, a]) = *px;
            let nr = (r as f32 * (1.0 - f) + 255.0 * f) as u8;
            let ng = (g as f32 * (1.0 - f) + 255.0 * f) as u8;
            let nb = (b as f32 * (1.0 - f) + 255.0 * f) as u8;
            *px = Rgba([nr, ng, nb, a]);
        }
    }

    // 图例：底部通栏流式条目
    if let Some(fnt) = font {
        draw_flow_legend(&mut main, items, fnt);
    }

    let mut buf = Vec::new();
    main.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .context("编码 PNG 失败")?;
    Ok(buf)
}

/// hex（如 "1D4ED8"）→ Rgba。
fn hex_to_rgba(hex: &str) -> Rgba<u8> {
    let v = u32::from_str_radix(hex, 16).unwrap_or(0x0F172A);
    Rgba([
        ((v >> 16) & 0xFF) as u8,
        ((v >> 8) & 0xFF) as u8,
        (v & 0xFF) as u8,
        255,
    ])
}

/// 清洗景点名：高德 labels 参数以 , : | ; 作结构字符，名称中出现会破坏格式。
fn clean_label_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if matches!(c, ',' | ':' | '|' | ';') {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// 交通方式 → 路线颜色（0xRRGGBB）。深色系：不与高德底图（灰白路/黄路/红拥堵/橙POI）撞色。
/// 公共交通=换乘方案判定色（原地铁蓝）；打车=驾车路线；步行骑行共用一色。
fn mode_color_hex(mode: Option<&str>) -> &'static str {
    match mode {
        Some("公共交通") | Some("地铁") | Some("公交") => "1D4ED8", // 深蓝
        Some("打车") | Some("驾车") => "4C1D95",                    // 深紫
        Some("步行骑行") | Some("步行") | Some("骑行") | Some("自行车") => "164E63", // 深青蓝
        _ => "0F172A",                                              // 未标注模式：近黑
    }
}

/// 交通方式显示名（图例用），未标注返回 None（不进图例）。
fn mode_display(mode: Option<&str>) -> Option<&'static str> {
    match mode {
        Some("公共交通") | Some("地铁") | Some("公交") => Some("公共交通"),
        Some("打车") | Some("驾车") => Some("打车"),
        Some("步行骑行") | Some("步行") | Some("骑行") | Some("自行车") => {
            Some("步行/骑行")
        }
        _ => None,
    }
}

/// 城市详细图工具：高德路径规划拿真实 polyline + 高德静态地图 API 服务端渲染。
/// 路线按交通方式固定配色；POI≥5 时总览用编号标记+图例；景点聚类后大簇追加放大图。
pub struct GenerateCityMap {
    amap_key: String,
    font_path: String,
    session_id: String,
    counter: AtomicU32,
    http: reqwest::Client,
    limiter: crate::tools::RateLimiter,
}

impl GenerateCityMap {
    /// counter 初值从 maps/{session_id}/ 已有图片的最大编号续接（重进旧会话不覆盖旧图）。
    /// http 客户端全工具复用（15s 超时）；高德请求经共享 RateLimiter 限流。
    pub fn new(
        amap_key: String,
        font_path: String,
        session_id: String,
        limiter: crate::tools::RateLimiter,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("构建 HTTP 客户端失败");
        Self {
            amap_key,
            font_path,
            counter: AtomicU32::new(super::scan_max_index(&session_id, "city")),
            session_id,
            http,
            limiter,
        }
    }

    /// 懒加载 CJK 字体（共用实现见 mod.rs）。
    fn load_font(&self) -> Option<ab_glyph::FontVec> {
        super::load_font(&self.font_path)
    }

    /// 主图与簇放大图共用的渲染流程，`pois` 决定本图范围：
    /// 只画两端都在范围内的路线；景点 ≥5 且有字体时用编号标记+编号图例（防标签重叠），
    /// 否则用名称标签。
    /// 路线合并后超过高德 paths 上限（4 组）时返回 Err，由调用方决定报错还是跳过该图。
    async fn draw_city_map(
        &self,
        pois: &[PoiNode],
        route_data: &[RouteData],
        font: Option<&ab_glyph::FontVec>,
    ) -> Result<Vec<u8>> {
        // 1. 只保留两端都在本图范围内的路线
        let names: std::collections::HashSet<&str> = pois.iter().map(|p| p.name.as_str()).collect();
        let routes: Vec<RouteData> = route_data
            .iter()
            .filter(|r| names.contains(r.from.as_str()) && names.contains(r.to.as_str()))
            .cloned()
            .collect();

        // 2. POI ≥5 且有字体时用编号标记（防名称标签重叠），否则用名称标签
        let numbered = pois.len() >= 5 && font.is_some();
        let (markers_param, labels_param) = if numbered {
            // 每个标记独立 style 组带编号 label（高德 label 支持单字符 0-9）
            let parts: Vec<String> = pois
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    let label = if i < 9 {
                        (i + 1).to_string()
                    } else {
                        "0".to_string()
                    };
                    format!("mid,0xDC2626,{label}:{:.6},{:.6}", p.lon, p.lat)
                })
                .collect();
            (parts.join("|"), String::new())
        } else {
            let m: Vec<String> = pois
                .iter()
                .map(|p| format!("{:.6},{:.6}", p.lon, p.lat))
                .collect();
            let markers = format!("mid,0xDC2626,:{}", m.join(";"));
            let labels: Vec<String> = pois
                .iter()
                .map(|p| {
                    format!(
                        "{},0,1,14,0xFFFFFF,0xDC2626:{:.6},{:.6}",
                        clean_label_name(&p.name),
                        p.lon,
                        p.lat
                    )
                })
                .collect();
            (markers, labels.join("|"))
        };

        // 3. 图例条目：标题 + 编号→名称（编号模式时）+ 本范围内用到的模式→颜色。
        //    条目在此一次性构建：实测高度给 plan_view 预留、绘制给 compose，两处同源。
        let mut items: Vec<LegendItem> = vec![LegendItem::Text("图例".into())];
        if numbered {
            // 编号对应高德 marker label：1..9，第10个是 0
            for (i, p) in pois.iter().enumerate() {
                let num = if i < 9 {
                    (i + 1).to_string()
                } else {
                    "0".to_string()
                };
                items.push(LegendItem::Text(format!("{num} {}", p.name)));
            }
        }
        let mut modes: Vec<(&'static str, &'static str)> = Vec::new();
        for r in &routes {
            if let Some(d) = mode_display(r.mode.as_deref()) {
                let c = mode_color_hex(r.mode.as_deref());
                if !modes.iter().any(|(name, _)| *name == d) {
                    modes.push((d, c));
                }
            }
        }
        for (mode, hex) in &modes {
            items.push(LegendItem::Swatch {
                color: hex_to_rgba(hex),
                text: (*mode).into(),
            });
        }
        // 图例实测高度（无字体/仅标题时为 0 → 不画图例也不预留）
        let legend_band = match font {
            Some(f) => super::flow_legend_band(&items, f, 800).max(0) as f64,
            None => 0.0,
        };

        // 4. bbox（全量点）→ 超大范围预检 → plan_view 定 zoom → 抽稀容差 ≈ 1px 对应度数。
        //    抽稀只影响塞进 URL 的渲染点，视图规划不受影响；主图/簇放大图各按自身 zoom 适配。
        let bbox = union_bbox(pois, &routes);
        if bbox_too_large(bbox, legend_band) {
            anyhow::bail!(
                "范围过大（可能跨省/跨大区）：城市图装不下。请改用 generate_overview_map 画总览，\
                 或把 pois 缩小到同一城市/片区后重试"
            );
        }
        let (zoom, _, _) = plan_view(bbox, legend_band);
        let tol_deg = 1.5 * 360.0 / (256.0 * 2.0_f64.powi(zoom as i32));

        // 5. paths 参数：按模式配色 + 连续同模式合并 + DP 抽稀 + URL 预算兜底
        let paths_param = build_paths_param_budgeted(&routes, tol_deg)?;

        // 6. 静态地图渲染 + 后处理（图例）。
        //    淡化系数 0：高德原图里标记/标签/路线与底图一体，淡化会把它们一起洗白，
        //    导致 POI 几乎不可见（实测 0.40 时标记红全被洗成浅粉）。
        let png = self
            .render_one(
                bbox,
                &paths_param,
                &markers_param,
                &labels_param,
                legend_band,
            )
            .await?;
        match compose_city_image(&png, 0.0, &items, font) {
            Ok(img) => Ok(img),
            Err(e) => {
                tracing::warn!("城市图后处理失败，使用原图: {e}");
                Ok(png)
            }
        }
    }

    /// 渲染一张城市图底图：plan_view 按「POI+路线」并集规划 zoom/中心（图例避让由几何保证）。
    /// legend_band 为图例实测预留高度（含下边距）。
    async fn render_one(
        &self,
        bbox: (f64, f64, f64, f64),
        paths_param: &str,
        markers_param: &str,
        labels_param: &str,
        legend_band: f64,
    ) -> Result<Vec<u8>> {
        let (zoom, loc_lon, loc_lat) = plan_view(bbox, legend_band);
        let location = format!("{loc_lon:.6},{loc_lat:.6}");
        tracing::debug!("[render_one] bbox={bbox:?} zoom={zoom} loc={location}");

        let png = self
            .fetch_staticmap(&location, zoom, paths_param, markers_param, labels_param)
            .await?;
        if png.len() < 4 || &png[..4] != b"\x89PNG" {
            let body = String::from_utf8_lossy(&png);
            anyhow::bail!("高德未返回图片，响应：{body}");
        }
        Ok(png)
    }

    /// 调高德路径规划，按交通方式分派 API。
    /// 返回 (polyline, 模式覆盖)：换乘（地铁/公交）会按最优方案内容判定模式
    /// （公共交通/打车/步行骑行）并覆盖 LLM 声明的 transport；其余模式沿用声明值（None）。
    async fn fetch_polyline(
        &self,
        mode: Option<&str>,
        origin: &str,
        dest: &str,
        city: &str,
    ) -> Result<(String, Option<String>)> {
        match mode {
            Some("地铁") | Some("公交") => {
                let (poly, m) = self.fetch_transit_polyline(origin, dest, city).await?;
                Ok((poly, Some(m.to_string())))
            }
            Some("步行") | Some("骑行") | Some("自行车") | Some("步行骑行") => Ok((
                self.fetch_path_polyline(
                    "https://restapi.amap.com/v3/direction/walking",
                    origin,
                    dest,
                )
                .await?,
                None,
            )),
            _ => Ok((
                self.fetch_path_polyline(
                    "https://restapi.amap.com/v3/direction/driving",
                    origin,
                    dest,
                )
                .await?,
                None,
            )),
        }
    }

    /// 驾车/步行路径规划（两个 API 响应结构一致：route.paths[].steps[].polyline）。
    async fn fetch_path_polyline(&self, url: &str, origin: &str, dest: &str) -> Result<String> {
        for attempt in 0..2 {
            self.limiter.wait().await;
            let resp = self
                .http_get(
                    url,
                    &[
                        ("key", self.amap_key.as_str()),
                        ("origin", origin),
                        ("destination", dest),
                        ("extensions", "all"),
                        ("output", "JSON"),
                    ],
                )
                .await?;
            let body: PathResp =
                serde_json::from_str(&resp).with_context(|| format!("解析路径响应失败: {resp}"))?;
            if body.status == "1" {
                let route = body
                    .route
                    .ok_or_else(|| anyhow::anyhow!("路径响应缺 route"))?;
                let path = route
                    .paths
                    .into_iter()
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("路径响应无 paths"))?;
                // 拼接所有 step 的 polyline
                let mut all_coords: Vec<&str> = Vec::new();
                for step in &path.steps {
                    all_coords.extend(step.polyline.split(';').filter(|s| !s.is_empty()));
                }
                return Ok(all_coords.join(";"));
            }
            // QPS 限流：等 1.2s 重试一次；其他错误直接报
            if body.info.contains("QPS") && attempt == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
                continue;
            }
            anyhow::bail!(
                "高德路径返回错误: status={} info={}",
                body.status,
                body.info
            );
        }
        // 循环内每条路径要么返回成功要么 bail；此处仅为类型完整性兜底（理论不可达）
        anyhow::bail!("高德路径请求失败：重试次数用尽")
    }

    /// 公交换乘路径规划（含地铁）：取最优（首个）方案，按行程顺序**全量拼接**方案内
    /// 所有路段——步行接驳 + 公交/地铁线路 + 打车段，首尾自然衔接出发地/目的地 POI。
    /// 整条路线的标记按方案内容判定（classify_transit_plan）：
    /// 含地铁/公交 → 公共交通；含打车 → 打车；仅步行 → 步行骑行。
    /// 返回 (polyline, 判定的模式)。QPS 限流自动重试一次。
    async fn fetch_transit_polyline(
        &self,
        origin: &str,
        dest: &str,
        city: &str,
    ) -> Result<(String, &'static str)> {
        for attempt in 0..2 {
            self.limiter.wait().await;
            let resp = self
                .http_get(
                    "https://restapi.amap.com/v3/direction/transit/integrated",
                    &[
                        ("key", self.amap_key.as_str()),
                        ("origin", origin),
                        ("destination", dest),
                        ("city", city),
                        ("extensions", "all"),
                        ("output", "JSON"),
                    ],
                )
                .await?;
            let body: TransitResp = serde_json::from_str(&resp)
                .with_context(|| format!("解析公交路径响应失败: {resp}"))?;
            if body.status == "1" {
                let route = body
                    .route
                    .ok_or_else(|| anyhow::anyhow!("公交路径响应缺 route"))?;
                let transit = route
                    .transits
                    .first()
                    .ok_or_else(|| anyhow::anyhow!("未找到可用公交/地铁方案"))?;
                // 全量拼接：按行程顺序收集步行/公交/打车各段 polyline（走最优高德路线）
                let mut coords: Vec<&str> = Vec::new();
                for seg in &transit.segments {
                    if let Some(w) = &seg.walking {
                        for s in &w.steps {
                            coords.extend(s.polyline.split(';').filter(|s| !s.is_empty()));
                        }
                    }
                    if let Some(b) = &seg.bus {
                        for l in &b.buslines {
                            coords.extend(l.polyline.split(';').filter(|s| !s.is_empty()));
                        }
                    }
                    if let Some(t) = &seg.taxi {
                        coords.extend(t.polyline.split(';').filter(|s| !s.is_empty()));
                    }
                }
                if coords.is_empty() {
                    anyhow::bail!(
                        "两地间无公共交通方案（线路不存在，或该段仅步行且无轨迹）。\
                         建议：①把此段 transport 改为「打车」重试 ②或从 routes 中移除该段后重画；不要用相同参数重试"
                    );
                }
                let mode = classify_transit_plan(transit);
                // 首尾衔接 POI（防个别方案缺步行接驳段时不连 POI）
                let mut full: Vec<String> = Vec::with_capacity(coords.len() + 2);
                full.push(origin.to_string());
                full.extend(coords.iter().map(|s| s.to_string()));
                full.push(dest.to_string());
                return Ok((full.join(";"), mode));
            }
            if body.info.contains("QPS") && attempt == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
                continue;
            }
            anyhow::bail!(
                "高德公交路径返回错误: status={} info={}",
                body.status,
                body.info
            );
        }
        anyhow::bail!("高德公交路径请求失败：重试次数用尽")
    }

    /// 调高德静态地图 API，返回 PNG 字节。
    async fn fetch_staticmap(
        &self,
        location: &str,
        zoom: u8,
        paths_param: &str,
        markers_param: &str,
        labels_param: &str,
    ) -> Result<Vec<u8>> {
        self.limiter.wait().await;
        let zoom_str = zoom.to_string();
        let mut params: Vec<(&str, &str)> = vec![
            ("size", "800*600"),
            ("location", location),
            ("zoom", &zoom_str),
            ("key", self.amap_key.as_str()),
        ];
        if !paths_param.is_empty() {
            params.push(("paths", paths_param));
        }
        if !markers_param.is_empty() {
            params.push(("markers", markers_param));
        }
        if !labels_param.is_empty() {
            params.push(("labels", labels_param));
        }
        let resp = self
            .http
            .get("https://restapi.amap.com/v3/staticmap")
            .query(&params)
            .send()
            .await
            .context("高德静态地图请求失败")?;
        if !resp.status().is_success() {
            let s = resp.status();
            let b = resp.text().await.unwrap_or_default();
            anyhow::bail!("高德静态地图请求失败 {s}: {b}");
        }
        Ok(resp.bytes().await?.to_vec())
    }

    async fn http_get(&self, url: &str, params: &[(&str, &str)]) -> Result<String> {
        let resp = self
            .http
            .get(url)
            .query(params)
            .send()
            .await
            .context("HTTP 请求失败")?;
        let text = resp.text().await.context("读取响应体失败")?;
        Ok(text)
    }
}

#[async_trait]
impl Tool for GenerateCityMap {
    fn name(&self) -> &str {
        "generate_city_map"
    }
    fn description(&self) -> &str {
        "生成城市详细图（高德真实地图底图 + 真实驾车路线 + 标记点）。\
         传入同一城市内的景点 POI 列表及游览路线，工具内部调高德路径规划+静态地图 API。\
         调用前请自行用 geocode/搜索解析出每个景点的经纬度并校验无误。"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "city": {"type": "string", "description": "城市名（用于命名图片）"},
                "pois": {
                    "type": "array",
                    "description": "景点/地点列表",
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
                "routes": {
                    "type": "array",
                    "description": "游览顺序路线段，from/to 引用 pois 里的 name。transport 标交通方式（决定路线颜色）",
                    "items": {
                        "type": "object",
                        "properties": {
                            "from": {"type": "string"},
                            "to": {"type": "string"},
                            "transport": {"type": "string", "description": "交通方式：地铁/公交/打车/步行骑行，不同方式不同颜色，不填为黑色"}
                        },
                        "required": ["from", "to"]
                    }
                }
            },
            "required": ["city", "pois", "routes"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String> {
        let city: String = args
            .get("city")
            .and_then(|v| v.as_str())
            .unwrap_or("未知城市")
            .to_string();
        let pois: Vec<PoiNode> =
            serde_json::from_value(args.get("pois").cloned().unwrap_or(Value::Null))
                .context("解析 pois 失败")?;
        let routes: Vec<RouteSeg> =
            serde_json::from_value(args.get("routes").cloned().unwrap_or(Value::Null))
                .context("解析 routes 失败")?;

        if pois.is_empty() {
            return Ok("城市图生成失败：pois 为空".into());
        }
        // routes 条数上限由 build_paths_param 在「连续同模式合并后」校验（高德 paths ≤4）；markers ≤10
        if pois.len() > 10 {
            return Ok(format!(
                "城市图生成失败：景点数 {} 超过高德静态地图上限（10），请精简",
                pois.len()
            ));
        }
        let names: std::collections::HashSet<&str> = pois.iter().map(|p| p.name.as_str()).collect();
        for (i, r) in routes.iter().enumerate() {
            if !names.contains(r.from.as_str()) {
                return Ok(format!(
                    "城市图生成失败：routes[{i}].from「{}」不在 pois 中",
                    r.from
                ));
            }
            if !names.contains(r.to.as_str()) {
                return Ok(format!(
                    "城市图生成失败：routes[{i}].to「{}」不在 pois 中",
                    r.to
                ));
            }
        }

        // 预检：与 build_paths_param 同规则的「连续同色合并」组数超上限时直接报错，
        // 避免先白花几十次路径规划请求、最后在渲染步才失败
        let groups = estimate_path_groups(&routes);
        if groups > 4 {
            return Ok(format!(
                "城市图生成失败：路线按交通方式合并后仍有 {groups} 组，\
                 超过高德静态地图上限（4），请减少路线或让相邻路段使用同一交通方式"
            ));
        }

        // name -> 坐标
        let coord_of: std::collections::HashMap<String, (f64, f64)> = pois
            .iter()
            .map(|p| (p.name.clone(), (p.lon, p.lat)))
            .collect();

        // 1. 每段路线调高德驾车路径规划 → RouteData（模式取 transport 字段）
        //    相邻请求间隔由共享 RateLimiter 保证（≥400ms，防 QPS 限流）
        let mut route_data: Vec<RouteData> = Vec::new();
        for r in routes.iter() {
            // execute 前置校验已确保路线两端都在 pois 中，此处理论不可达
            let (olon, olat) = coord_of
                .get(&r.from)
                .copied()
                .ok_or_else(|| anyhow::anyhow!("内部错误：「{}」不在坐标表中", r.from))?;
            let (dlon, dlat) = coord_of
                .get(&r.to)
                .copied()
                .ok_or_else(|| anyhow::anyhow!("内部错误：「{}」不在坐标表中", r.to))?;
            let origin = format!("{olon:.6},{olat:.6}");
            let dest = format!("{dlon:.6},{dlat:.6}");
            let (raw, mode_override) = match self
                .fetch_polyline(r.transport.as_deref(), &origin, &dest, &city)
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    return Ok(format!(
                        "城市图生成失败：路线「{}→{}」路径规划失败：{e}",
                        r.from, r.to
                    ));
                }
            };
            route_data.push(RouteData {
                from: r.from.clone(),
                to: r.to.clone(),
                mode: mode_override.or_else(|| r.transport.clone()),
                polyline: raw,
            });
        }

        // 2. 路线数据齐了，渲染统一走 draw_city_map（主图/簇放大图共用，仅 pois 范围不同）
        let font = self.load_font();
        let dir = std::path::Path::new("maps").join(sanitize(&self.session_id));
        std::fs::create_dir_all(&dir).context("创建 maps 目录失败")?;
        let mut generated: Vec<String> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();

        // 主图；渲染失败（如路线组数超上限）→ 整体报错。
        // 编号在主图成功后才消耗，失败重试不空跳编号
        let main_png = match self.draw_city_map(&pois, &route_data, font.as_ref()).await {
            Ok(v) => v,
            Err(e) => return Ok(format!("城市图生成失败：{e}")),
        };
        let n = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
        let path = dir.join(format!("city_{n}.png"));
        std::fs::write(&path, &main_png)
            .with_context(|| format!("保存 {} 失败", path.display()))?;
        generated.push(path.display().to_string());

        // 3. 聚类：总簇数 ≥2 时，≥4 个景点的簇追加放大图（上限 3 张）
        // 阈值自适应尺度：Diameter × (1 - 1/1.5) = D/3（D=最远两点距离）。
        // 市区级 D≈10km → 阈值 3.3km；跨省 D≈300km → 阈值 100km，天然适配。
        const CLUSTER_X: f64 = 1.5;
        const MIN_CLUSTER: usize = 4;
        const MAX_CLUSTER_MAPS: usize = 3;
        let diameter = pois
            .iter()
            .enumerate()
            .flat_map(|(i, a)| {
                pois.iter()
                    .skip(i + 1)
                    .map(move |b| dist_km(a.lon, a.lat, b.lon, b.lat))
            })
            .fold(0.0_f64, f64::max);
        let threshold_km = diameter * (1.0 - 1.0 / CLUSTER_X);
        let clusters = single_link_clusters(
            pois.len(),
            |i, j| dist_km(pois[i].lon, pois[i].lat, pois[j].lon, pois[j].lat),
            threshold_km,
        );
        if clusters.len() >= 2 {
            let mut made = 0;
            for cl in &clusters {
                if cl.len() < MIN_CLUSTER || made >= MAX_CLUSTER_MAPS {
                    continue;
                }
                let sub_pois: Vec<PoiNode> = cl.iter().map(|&i| pois[i].clone()).collect();
                match self
                    .draw_city_map(&sub_pois, &route_data, font.as_ref())
                    .await
                {
                    Ok(png) => {
                        made += 1;
                        let cpath = dir.join(format!("city_{n}_c{made}.png"));
                        if std::fs::write(&cpath, &png).is_ok() {
                            generated.push(cpath.display().to_string());
                        }
                    }
                    Err(e) => skipped.push(format!("簇放大图（{} 个景点）未生成：{e}", cl.len())),
                }
            }
        }

        let mut out = format!(
            "已生成城市详细图「{city}」：{}（{} 个景点、{} 条路线）",
            generated.join("、"),
            pois.len(),
            routes.len()
        );
        for s in &skipped {
            out.push_str(&format!("\n{s}"));
        }
        Ok(out)
    }
}

/// 景点节点（城市详细图用）。
#[derive(Deserialize, Clone)]
struct PoiNode {
    name: String,
    lon: f64,
    lat: f64,
}

/// 一段已取回 polyline 的路线（城市图内部用）。
#[derive(Clone)]
struct RouteData {
    from: String,
    to: String,
    mode: Option<String>,
    polyline: String,
}

/// 渲染前预检：按 build_paths_param 完全相同的「连续同色合并」规则，
/// 纯凭路线序列（不发任何网络请求）预估合并后的 paths 组数。
fn estimate_path_groups(routes: &[RouteSeg]) -> usize {
    let mut groups = 0usize;
    let mut cur_color = "";
    let mut cur_to = String::new();
    let mut started = false; // 等价于 build_paths_param 的 cur_poly 非空
    for r in routes {
        let color = mode_color_hex(r.transport.as_deref());
        let continuous = started && cur_to == r.from && color == cur_color;
        if !continuous {
            groups += 1;
            cur_color = color;
        }
        cur_to = r.to.clone();
        started = true;
    }
    groups
}

/// polyline 字符串（"lon,lat;lon,lat…"）→ 坐标点列表，忽略无法解析的段。
fn parse_polyline(s: &str) -> Vec<(f64, f64)> {
    s.split(';')
        .filter(|p| !p.is_empty())
        .filter_map(|p| {
            let mut it = p.split(',');
            let lon: f64 = it.next()?.trim().parse().ok()?;
            let lat: f64 = it.next()?.trim().parse().ok()?;
            Some((lon, lat))
        })
        .collect()
}

/// 坐标点列表 → polyline 字符串（统一 6 位小数，与高德原始精度一致）。
fn polyline_to_string(pts: &[(f64, f64)]) -> String {
    pts.iter()
        .map(|(lon, lat)| format!("{lon:.6},{lat:.6}"))
        .collect::<Vec<_>>()
        .join(";")
}

/// Douglas-Peucker 折线抽稀（度空间，城市尺度足够）。tol=0 仅去除共线/重复点；
/// 端点恒保留，相邻段共享的接驳点因此不会丢。
fn dp_simplify(pts: &[(f64, f64)], tol: f64) -> Vec<(f64, f64)> {
    if pts.len() <= 2 {
        return pts.to_vec();
    }
    let tol2 = tol * tol;
    let mut keep = vec![false; pts.len()];
    keep[0] = true;
    keep[pts.len() - 1] = true;
    let mut stack = vec![(0usize, pts.len() - 1)];
    while let Some((a, b)) = stack.pop() {
        if b <= a + 1 {
            continue;
        }
        let (ax, ay) = pts[a];
        let (bx, by) = pts[b];
        let (dx, dy) = (bx - ax, by - ay);
        let seg2 = dx * dx + dy * dy;
        let mut best = 0.0_f64;
        let mut best_i = a;
        for (i, &(px, py)) in pts.iter().enumerate().take(b).skip(a + 1) {
            let d2 = if seg2 == 0.0 {
                (px - ax).powi(2) + (py - ay).powi(2)
            } else {
                let t = (((px - ax) * dx + (py - ay) * dy) / seg2).clamp(0.0, 1.0);
                let (qx, qy) = (ax + t * dx, ay + t * dy);
                (px - qx).powi(2) + (py - qy).powi(2)
            };
            if d2 > best {
                best = d2;
                best_i = i;
            }
        }
        if best > tol2 {
            keep[best_i] = true;
            stack.push((a, best_i));
            stack.push((best_i, b));
        }
    }
    pts.iter()
        .enumerate()
        .filter(|(i, _)| keep[*i])
        .map(|(_, p)| *p)
        .collect()
}

/// paths 参数长度预算：高德网关（Tengine）对 GET URL 约 8KB 上限，
/// 其余参数（key/location/markers 等）数百字节，paths 控制在 6KB 内。
const PATHS_BUDGET: usize = 6000;
/// 预算兜底最多加倍容差次数（1e-4 起步翻 8 次到 2.56e-2 ≈ 2.8km，仍超则报错）。
const BUDGET_MAX_ITER: u32 = 8;

/// 按容差构建 paths 参数，超 URL 预算时自动加倍容差重试。
/// 渲染是示意线，偏差 1~2px 不可感知；bbox/zoom 规划仍用全量点，视图不受影响。
fn build_paths_param_budgeted(routes: &[RouteData], base_tol: f64) -> Result<String> {
    let mut tol = base_tol;
    for _ in 0..=BUDGET_MAX_ITER {
        let s = build_paths_param(routes, tol)?;
        if s.len() <= PATHS_BUDGET {
            return Ok(s);
        }
        tol = if tol <= 0.0 { 1e-4 } else { tol * 2.0 };
    }
    anyhow::bail!("路线坐标抽稀后 paths 参数仍超 URL 长度预算（{PATHS_BUDGET} 字节），请减少路线")
}

/// 把连续同模式的路线段合并成高德 paths 参数（每条 path 一种颜色，上限 4 条）。
/// 仅当上一段的 to == 下一段的 from（行程连续）且模式相同才合并，避免不连续段被直线连接。
/// tol_deg > 0 时对每段 polyline 做 DP 抽稀（端点保留，接驳连续性不受影响）。
fn build_paths_param(routes: &[RouteData], tol_deg: f64) -> Result<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut cur_color = "";
    let mut cur_poly = String::new();
    let mut cur_to = String::new(); // 当前累积段结束的景点名
    for r in routes {
        let color = mode_color_hex(r.mode.as_deref());
        let continuous = !cur_poly.is_empty() && cur_to == r.from && color == cur_color;
        if !cur_poly.is_empty() && !continuous {
            parts.push(format!("8,0x{cur_color},0.9,,:{cur_poly}"));
            cur_poly.clear();
        }
        let seg = polyline_to_string(&dp_simplify(&parse_polyline(&r.polyline), tol_deg));
        if cur_poly.is_empty() {
            cur_color = color;
            cur_poly = seg;
        } else {
            cur_poly.push(';');
            cur_poly.push_str(&seg);
        }
        cur_to = r.to.clone();
    }
    if !cur_poly.is_empty() {
        parts.push(format!("8,0x{cur_color},0.9,,:{cur_poly}"));
    }
    if parts.len() > 4 {
        anyhow::bail!(
            "合并后仍有 {} 条路线，超过高德静态地图上限（4）",
            parts.len()
        );
    }
    Ok(parts.join("|"))
}

// ---- 高德路径规划响应类型（驾车/步行共用 PathResp；公交换乘 TransitResp） ----

#[derive(Deserialize)]
struct PathResp {
    status: String,
    #[serde(default)]
    info: String,
    route: Option<PathRoute>,
}

#[derive(Deserialize)]
struct PathRoute {
    #[serde(default)]
    paths: Vec<PathRoutePath>,
}

#[derive(Deserialize)]
struct PathRoutePath {
    #[serde(default, rename = "steps")]
    steps: Vec<PathStep>,
}

#[derive(Deserialize)]
struct PathStep {
    #[serde(default)]
    polyline: String,
}

// ---- 公交换乘（含地铁）响应 ----

#[derive(Deserialize)]
struct TransitResp {
    status: String,
    #[serde(default)]
    info: String,
    route: Option<TransitRoute>,
}

#[derive(Deserialize)]
struct TransitRoute {
    #[serde(default)]
    transits: Vec<TransitPlan>,
}

#[derive(Deserialize)]
struct TransitPlan {
    #[serde(default)]
    segments: Vec<TransitSegment>,
}

#[derive(Deserialize)]
struct TransitSegment {
    #[serde(default)]
    walking: Option<TransitWalking>,
    #[serde(default)]
    bus: Option<TransitBus>,
    #[serde(default)]
    taxi: Option<TransitTaxi>,
}

#[derive(Deserialize)]
struct TransitWalking {
    #[serde(default)]
    steps: Vec<PathStep>,
}

#[derive(Deserialize)]
struct TransitTaxi {
    #[serde(default)]
    polyline: String,
}

#[derive(Deserialize)]
struct TransitBus {
    #[serde(default)]
    buslines: Vec<TransitBusLine>,
}

#[derive(Deserialize)]
struct TransitBusLine {
    #[serde(default)]
    name: Option<String>,
    #[serde(default, rename = "type")]
    line_type: Option<String>,
    #[serde(default)]
    polyline: String,
}

/// 换乘方案属性判定（按优先级）：方案含公共交通线路（线路 type/name 含
/// 「地铁/公交/轨道/铁路/号线」字样）→ 公共交通；含打车段 → 打车；仅步行 → 步行骑行。
/// 线路字段缺失时按「buslines 存在即公共交通」兜底。
fn classify_transit_plan(transit: &TransitPlan) -> &'static str {
    let mut has_pub = false;
    let mut has_taxi = false;
    for seg in &transit.segments {
        if let Some(b) = &seg.bus {
            for l in &b.buslines {
                if l.polyline.is_empty() {
                    continue;
                }
                let hay = format!(
                    "{} {}",
                    l.line_type.as_deref().unwrap_or(""),
                    l.name.as_deref().unwrap_or("")
                );
                let keywords = ["地铁", "公交", "轨道", "铁路", "号线"];
                if keywords.iter().any(|k| hay.contains(k)) || hay.trim().is_empty() {
                    has_pub = true;
                }
            }
        }
        if let Some(t) = &seg.taxi
            && !t.polyline.is_empty()
        {
            has_taxi = true;
        }
    }
    if has_pub {
        "公共交通"
    } else if has_taxi {
        "打车"
    } else {
        "步行骑行"
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{test_amap_key, test_font_path};
    use super::*;

    /// 实跑：画一张北京市内景点详细图。
    /// 运行：`cargo test city_map_real -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn city_map_real() {
        let tool = GenerateCityMap::new(
            test_amap_key(),
            test_font_path(),
            "test".into(),
            crate::tools::RateLimiter::new(400),
        );
        let args = serde_json::json!({
            "city": "北京",
            "pois": [
                {"name": "天安门", "lon": 116.397428, "lat": 39.90923},
                {"name": "故宫", "lon": 116.403654, "lat": 39.915156},
                {"name": "景山公园", "lon": 116.402687, "lat": 39.924847},
                {"name": "北海公园", "lon": 116.389706, "lat": 39.925526}
            ],
            "routes": [
                {"from": "天安门", "to": "故宫"},
                {"from": "故宫", "to": "景山公园"},
                {"from": "景山公园", "to": "北海公园"}
            ]
        });
        let out = tool.execute(args).await.expect("execute 失败");
        println!("\n=== generate_city_map 输出 ===\n{out}\n");
        assert!(out.contains("已生成城市详细图"), "输出异常: {out}");
    }

    /// 实跑：郑州黄河文化公园→只有河南城市图。
    /// 运行：`cargo test city_map_zzbj -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn city_map_zzbj() {
        let tool = GenerateCityMap::new(
            test_amap_key(),
            test_font_path(),
            "test".into(),
            crate::tools::RateLimiter::new(400),
        );
        let args = serde_json::json!({
            "city": "郑州",
            "pois": [
                {"name": "黄河文化公园", "lon": 113.612, "lat": 34.907},
                {"name": "只有河南", "lon": 114.004, "lat": 34.800}
            ],
            "routes": [
                {"from": "黄河文化公园", "to": "只有河南"}
            ]
        });
        let out = tool.execute(args).await.expect("execute 失败");
        println!("\n=== 郑州城市图 ===\n{out}\n");
        assert!(out.contains("已生成城市详细图"), "输出异常: {out}");
    }

    /// 实跑：郑州 7 景点密集+远端分布（编号标记 + 模式配色 + 聚类放大图）。
    /// 市区簇：二七纪念塔/郑州站/紫荆山公园/河南博物院（≥4 应出放大图）；
    /// 远端：只有河南/电影小镇（2个，不出图）、黄河文化公园（1个，不出图）。
    /// 运行：`cargo test city_map_zz_dense -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn city_map_zz_dense() {
        let tool = GenerateCityMap::new(
            test_amap_key(),
            test_font_path(),
            "test".into(),
            crate::tools::RateLimiter::new(400),
        );
        let args = serde_json::json!({
            "city": "郑州",
            "pois": [
                {"name": "郑州站", "lon": 113.6416, "lat": 34.7466},
                {"name": "二七纪念塔", "lon": 113.6609, "lat": 34.7522},
                {"name": "紫荆山公园", "lon": 113.6796, "lat": 34.7643},
                {"name": "河南博物院", "lon": 113.6856, "lat": 34.7891},
                {"name": "电影小镇", "lon": 113.9880, "lat": 34.7780},
                {"name": "只有河南", "lon": 114.0036, "lat": 34.8004},
                {"name": "黄河文化公园", "lon": 113.6120, "lat": 34.9070}
            ],
            "routes": [
                {"from": "郑州站", "to": "二七纪念塔", "transport": "地铁"},
                {"from": "二七纪念塔", "to": "紫荆山公园", "transport": "地铁"},
                {"from": "紫荆山公园", "to": "河南博物院", "transport": "步行骑行"},
                {"from": "河南博物院", "to": "电影小镇", "transport": "打车"},
                {"from": "电影小镇", "to": "只有河南", "transport": "打车"},
                {"from": "只有河南", "to": "黄河文化公园", "transport": "打车"}
            ]
        });
        let out = tool.execute(args).await.expect("execute 失败");
        println!("\n=== 郑州密集分布城市图 ===\n{out}\n");
        assert!(out.contains("已生成城市详细图"), "输出异常: {out}");
        // 应生成主图 + 1 张市区簇放大图
        assert!(out.contains("_c1"), "应生成簇放大图: {out}");
    }

    /// 诊断：实测高德静态地图的真实比例（zoom 语义）。
    /// 已知图中心放一个已知坐标的红色 marker，用红色像素反推实际像素位置，
    /// 与 Web Mercator 256px 瓦片模型的预测对比，输出比例系数 k。
    /// 运行：`cargo test staticmap_scale_probe -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn staticmap_scale_probe() {
        let tool = GenerateCityMap::new(
            test_amap_key(),
            test_font_path(),
            "probe".into(),
            crate::tools::RateLimiter::new(400),
        );
        let (lon0, lat0) = (116.407_f64, 39.904_f64);
        let (lon1, lat1) = (116.397428_f64, 39.90923_f64); // 天安门
        let zoom = 13_u8;
        // 按实际渲染比例（tile = zoom + AMAP_ZOOM_BIAS）预测
        let t = zoom + AMAP_ZOOM_BIAS;
        let x0 = staticmap::lon_to_x(lon0, t);
        let y0 = staticmap::lat_to_y(lat0, t);
        let x1 = staticmap::lon_to_x(lon1, t);
        let y1 = staticmap::lat_to_y(lat1, t);
        let pred = ((x1 - x0) * 256.0, (y1 - y0) * 256.0);
        let png = tool
            .fetch_staticmap(
                &format!("{lon0},{lat0}"),
                zoom,
                "",
                &format!("mid,0xDC2626,:{lon1},{lat1}"),
                "",
            )
            .await
            .expect("fetch 失败");
        let img = image::load_from_memory(&png).expect("解码失败").to_rgba8();
        let (mut sx, mut sy, mut n) = (0.0_f64, 0.0_f64, 0_u64);
        let mut tip_y = 0_u32;
        for (x, y, p) in img.enumerate_pixels() {
            let Rgba([r, g, b, _]) = *p;
            if r > 200 && g < 80 && b < 80 {
                sx += x as f64;
                sy += y as f64;
                n += 1;
                tip_y = tip_y.max(y);
            }
        }
        assert!(n > 0, "没找到红色 marker 像素");
        let cx = sx / n as f64;
        let ax = cx - 400.0;
        // balloon 锚点在尖端（质心下方），取质心附近列带的最大 y 近似
        for (x, y, p) in img.enumerate_pixels() {
            let Rgba([r, g, b, _]) = *p;
            if r > 200 && g < 80 && b < 80 && (x as f64 - cx).abs() < 8.0 {
                tip_y = tip_y.max(y);
            }
        }
        let ay_centroid = sy / n as f64 - 300.0;
        let ay_tip = tip_y as f64 - 300.0;
        println!("模型预测偏移 px = ({:.1}, {:.1})", pred.0, pred.1);
        println!(
            "实际偏移 px：x质心 {ax:.1}，y质心 {ay_centroid:.1}，y尖端 {ay_tip:.1}（红像素 {n}）"
        );
        println!(
            "比例系数 kx = {:.3}，ky(尖端) = {:.3}",
            ax / pred.0,
            ay_tip / pred.1
        );
    }

    /// estimate_path_groups：与 build_paths_param 同规则的组数预估
    /// （连续同色合并；打车/驾车同色、步行/骑行同色；断链即分组）。
    #[test]
    fn estimate_path_groups_basic() {
        let seg = |from: &str, to: &str, t: Option<&str>| RouteSeg {
            from: from.into(),
            to: to.into(),
            transport: t.map(|s| s.into()),
        };
        // 连续地铁×2 + 打车 → 2 组
        let segs = vec![
            seg("A", "B", Some("地铁")),
            seg("B", "C", Some("地铁")),
            seg("C", "D", Some("打车")),
        ];
        assert_eq!(estimate_path_groups(&segs), 2);
        // 打车/驾车同色 → 合并为 1 组
        let segs = vec![seg("A", "B", Some("打车")), seg("B", "C", Some("驾车"))];
        assert_eq!(estimate_path_groups(&segs), 1);
        // 断链（前段终点 B ≠ 后段起点 C）→ 分 2 组
        let segs = vec![seg("A", "B", Some("地铁")), seg("C", "D", Some("地铁"))];
        assert_eq!(estimate_path_groups(&segs), 2);
        assert_eq!(estimate_path_groups(&[]), 0);
    }

    /// bbox_too_large：城市级/大都市圈 bbox 通过；跨省（~40°）与全球级触发（非城市尺度）。
    #[test]
    fn bbox_too_large_rules() {
        assert!(!bbox_too_large((113.60, 114.05, 34.74, 34.92), 100.0));
        // 延吉+图们 ~0.4°：通过
        assert!(!bbox_too_large((129.47, 129.86, 42.86, 43.00), 100.0));
        // 长春→乌鲁木齐 ~38°：非城市尺度，触发
        assert!(bbox_too_large((87.0, 125.0, 31.0, 44.0), 100.0));
        // 全球级：触发
        assert!(bbox_too_large((0.0, 179.0, 10.0, 50.0), 100.0));
    }

    /// dp_simplify 纯函数：共线密点坍缩、拐点按容差保留、端点恒在。
    #[test]
    fn dp_simplify_rules() {
        // 对角直线上的 101 个点 → 只剩两端
        let line: Vec<(f64, f64)> = (0..=100)
            .map(|i| (i as f64 / 100.0, i as f64 / 100.0))
            .collect();
        let out = dp_simplify(&line, 1e-3);
        assert_eq!(out.len(), 2, "共线点应全部坍缩");
        // 拐点偏离弦 0.01：容差 0.005 保留、0.02 坍缩
        let bend = vec![(0.0, 0.0), (0.01, 0.0), (0.02, 0.01), (0.03, 0.0)];
        assert_eq!(dp_simplify(&bend, 0.005).len(), 3, "超容差拐点应保留");
        assert_eq!(dp_simplify(&bend, 0.02).len(), 2, "容差内拐点应坍缩");
        // 端点与退化输入
        assert_eq!(dp_simplify(&bend, 0.005)[0], (0.0, 0.0));
        assert_eq!(dp_simplify(&bend, 0.005)[2], (0.03, 0.0));
        assert_eq!(dp_simplify(&[], 1e-3).len(), 0);
        assert_eq!(dp_simplify(&[(1.0, 2.0)], 1e-3).len(), 1);
        // tol=0 只去共线点，不破坏折点
        let zig = vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0)];
        assert_eq!(dp_simplify(&zig, 0.0).len(), 3);
    }

    /// paths 参数 URL 预算兜底：密点正弦折线应被抽到预算内且保端点；
    /// 短参数原样通过；极端锯齿（抽不进预算）报清晰错误。
    #[test]
    fn paths_param_budget_rules() {
        let rd = |polyline: String| RouteData {
            from: "A".into(),
            to: "B".into(),
            mode: Some("打车".into()),
            polyline,
        };
        // 2000 点正弦抖动 ≈ 40KB 原始参数 → 抽稀后须落回预算内，端点保留
        let pts: Vec<(f64, f64)> = (0..2000)
            .map(|i| {
                let t = i as f64;
                (113.0 + t * 1e-4, 34.0 + (t / 7.0).sin() * 5e-3)
            })
            .collect();
        let s = build_paths_param_budgeted(&[rd(polyline_to_string(&pts))], 1e-6)
            .expect("预算构建失败");
        assert!(s.len() <= PATHS_BUDGET, "应 ≤ 预算，实际 {}", s.len());
        let (first, last) = (pts[0], pts[pts.len() - 1]);
        assert!(s.contains(&polyline_to_string(&[first])), "起点应保留");
        assert!(s.contains(&polyline_to_string(&[last])), "终点应保留");
        // 短参数（远低于预算）不被抽稀破坏：与 tol=0 直构结果一致
        let short: Vec<(f64, f64)> = vec![(113.0, 34.0), (113.01, 34.008), (113.02, 34.0)];
        let a = build_paths_param_budgeted(&[rd(polyline_to_string(&short))], 1e-6).unwrap();
        let b = build_paths_param(&[rd(polyline_to_string(&short))], 0.0).unwrap();
        assert_eq!(a, b);
        // 幅度 0.2° 的锯齿在最大容差下仍无法压缩 → 报错而非静默截断
        let zig: Vec<(f64, f64)> = (0..2000)
            .map(|i| {
                (
                    113.0 + i as f64 * 1e-4,
                    34.0 + if i % 2 == 0 { 0.1 } else { -0.1 },
                )
            })
            .collect();
        assert!(build_paths_param_budgeted(&[rd(polyline_to_string(&zig))], 1e-6).is_err());
    }

    /// plan_view 几何保证：「POI+路线」并集包围盒的投影像素必须整体落在
    /// 边缘安全区 − 底部图例带的自由矩形内，且水平居中。
    #[test]
    fn view_plan_keeps_geometry_clear_of_legend() {
        // 模拟跨城 misuse+远端路线：东西 0.45°、南北 0.18°（约 40km×20km）
        let bbox = (113.60_f64, 114.05_f64, 34.74_f64, 34.92_f64);
        let (zoom, loc_lon, loc_lat) = plan_view(bbox, 100.0);
        assert!((1..=17).contains(&zoom));
        // 复现静态地图的投影模型（含 AMAP_ZOOM_BIAS）：location 对应图中心 (400,300)
        let t = zoom + AMAP_ZOOM_BIAS;
        let to_px = |lon: f64, lat: f64| -> (f64, f64) {
            (
                400.0 + (staticmap::lon_to_x(lon, t) - staticmap::lon_to_x(loc_lon, t)) * 256.0,
                300.0 + (staticmap::lat_to_y(lat, t) - staticmap::lat_to_y(loc_lat, t)) * 256.0,
            )
        };
        // Mercator 对 lon/lat 均单调，四角投影即包围盒投影范围
        let (min_lon, max_lon, min_lat, max_lat) = bbox;
        let corners = [
            (min_lon, min_lat),
            (max_lon, max_lat),
            (min_lon, max_lat),
            (max_lon, min_lat),
        ];
        for (lon, lat) in corners {
            let (px, py) = to_px(lon, lat);
            assert!(
                (24.0..=776.0).contains(&px),
                "lon={lon} 投影 px={px} 超出边缘安全区"
            );
            // 竖直：上不出边缘安全区（24），下不进底部图例通栏（600-100=500，留边距 476）
            assert!(
                (24.0..=476.0).contains(&py),
                "lat={lat} 投影 py={py} 侵入边缘或图例带"
            );
        }
        // 内容中心应水平居中、竖直居中于可用区（可用区 = 边缘安全区扣除图例带）
        let (px, py) = to_px((min_lon + max_lon) / 2.0, (min_lat + max_lat) / 2.0);
        assert!((px - 400.0).abs() < 1.0, "水平应居中，实际 px={px}");
        assert!(
            ((24.0 + 476.0) / 2.0 - py).abs() < 1.0,
            "竖直应居中于可用区，实际 py={py}"
        );
    }

    /// 实跑：地铁模式（公交换乘 API）+ 步行模式混合路线，验证按交通方式分派。
    /// 运行：`cargo test city_map_transit -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn city_map_transit() {
        let tool = GenerateCityMap::new(
            test_amap_key(),
            test_font_path(),
            "test".into(),
            crate::tools::RateLimiter::new(400),
        );
        let args = serde_json::json!({
            "city": "郑州",
            "pois": [
                {"name": "郑州站", "lon": 113.6416, "lat": 34.7466},
                {"name": "二七纪念塔", "lon": 113.6609, "lat": 34.7522},
                {"name": "紫荆山公园", "lon": 113.6796, "lat": 34.7643},
                {"name": "河南博物院", "lon": 113.6856, "lat": 34.7891}
            ],
            "routes": [
                {"from": "郑州站", "to": "二七纪念塔", "transport": "步行"},
                {"from": "二七纪念塔", "to": "紫荆山公园", "transport": "地铁"},
                {"from": "紫荆山公园", "to": "河南博物院", "transport": "步行骑行"}
            ]
        });
        let out = tool.execute(args).await.expect("execute 失败");
        println!("\n=== 地铁/步行混合城市图 ===\n{out}\n");
        assert!(out.contains("已生成城市详细图"), "输出异常: {out}");
    }

    /// 实跑：北京五道口→东直门 地铁城市路线图（13号线场景）。
    /// 验证：换乘步行接驳 <2km 不画线，只画地铁主干。
    /// 运行：`cargo test city_map_wdk_dzm -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn city_map_wdk_dzm() {
        let tool = GenerateCityMap::new(
            test_amap_key(),
            test_font_path(),
            "test".into(),
            crate::tools::RateLimiter::new(400),
        );
        let args = serde_json::json!({
            "city": "北京",
            "pois": [
                {"name": "五道口", "lon": 116.33797, "lat": 39.99257},
                {"name": "东直门", "lon": 116.43427, "lat": 39.94163}
            ],
            "routes": [
                {"from": "五道口", "to": "东直门", "transport": "地铁"}
            ]
        });
        let out = tool.execute(args).await.expect("execute 失败");
        println!("\n=== 五道口→东直门 地铁城市图 ===\n{out}\n");
        assert!(out.contains("已生成城市详细图"), "输出异常: {out}");
    }

    /// 诊断：五道口→东直门公交换乘 polyline，验证：
    /// ① 首尾点分别是出发地/目的地 POI（整条路线连成一条）；
    /// ② 中间为单模式主干段（本场景为地铁，不含步行接驳点）。
    /// 运行：`cargo test transit_polyline_probe -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn transit_polyline_probe() {
        let tool = GenerateCityMap::new(
            test_amap_key(),
            test_font_path(),
            "probe".into(),
            crate::tools::RateLimiter::new(400),
        );
        let (raw, mode) = tool
            .fetch_transit_polyline("116.337970,39.992570", "116.434270,39.941630", "北京")
            .await
            .expect("换乘路径规划失败");
        let pts: Vec<&str> = raw.split(';').collect();
        println!("判定模式 = {mode}");
        println!("polyline 点数 = {}", pts.len());
        println!("首点 = {}", pts.first().unwrap_or(&"?"));
        println!("末点 = {}", pts.last().unwrap_or(&"?"));
        println!("前 3 点 = {:?}", &pts[..pts.len().min(3)]);
        println!("后 3 点 = {:?}", &pts[pts.len().saturating_sub(3)..]);
        assert_eq!(
            pts.first().copied(),
            Some("116.337970,39.992570"),
            "首点应为出发地 POI"
        );
        assert_eq!(
            pts.last().copied(),
            Some("116.434270,39.941630"),
            "末点应为目的地 POI"
        );
        assert!(pts.len() > 3, "polyline 过短");
    }

    /// 诊断：二分复现 c1 卡边问题——同一 zoom/location，分别用
    /// ①完整 paths+markers+labels ②无labels ③无paths ④仅markers 请求，
    /// 对比红像素纵向分布，定位是哪个参数导致渲染比例异常。
    /// 运行：`cargo test c1_repro_probe -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn c1_repro_probe() {
        let tool = GenerateCityMap::new(
            test_amap_key(),
            test_font_path(),
            "probe".into(),
            crate::tools::RateLimiter::new(400),
        );
        let pois = [
            (113.6416_f64, 34.7466_f64, "郑州站"),
            (113.6609_f64, 34.7522_f64, "二七纪念塔"),
            (113.6796_f64, 34.7643_f64, "紫荆山公园"),
            (113.6856_f64, 34.7891_f64, "河南博物院"),
        ];
        // 拉真实路线（与 c1 相同的三段）
        let mut rds: Vec<RouteData> = Vec::new();
        for (mode_str, a, b) in [("地铁", 0usize, 1usize), ("地铁", 1, 2), ("步行骑行", 2, 3)]
        {
            let (x1, y1, n1) = pois[a];
            let (x2, y2, n2) = pois[b];
            let (raw, mode) = tool
                .fetch_polyline(
                    Some(mode_str),
                    &format!("{x1},{y1}"),
                    &format!("{x2},{y2}"),
                    "郑州",
                )
                .await
                .expect("路径规划失败");
            rds.push(RouteData {
                from: n1.into(),
                to: n2.into(),
                mode: Some(mode.unwrap_or_else(|| mode_str.to_string())),
                polyline: raw,
            });
        }
        let paths = build_paths_param(&rds, 0.0).expect("paths 构建失败");
        let m = "mid,0xDC2626,:113.6416,34.7466;113.6609,34.7522;113.6796,34.7643;113.6856,34.7891";
        let l = pois
            .iter()
            .map(|(x, y, n)| format!("{n},0,1,14,0xFFFFFF,0xDC2626:{x},{y}"))
            .collect::<Vec<_>>()
            .join("|");
        let loc = "113.663818,34.768056";
        for (tag, p, mk, lb) in [
            ("①完整", paths.as_str(), m, l.as_str()),
            ("②无labels", paths.as_str(), m, ""),
            ("③无paths", "", m, l.as_str()),
            ("④仅markers", "", m, ""),
        ] {
            let png = tool
                .fetch_staticmap(loc, 13, p, mk, lb)
                .await
                .expect("静态图请求失败");
            std::fs::create_dir_all("maps/test").unwrap();
            let save = format!("maps/test/repro_{tag}.png");
            std::fs::write(&save, &png).unwrap();
            let img = image::load_from_memory(&png).expect("解码失败").to_rgba8();
            let mut red = [0u64; 12];
            let mut redx = [0u64; 16];
            let (mut bny, mut bxy) = (u32::MAX, 0u32);
            let mut b_n = 0u64;
            for (x, y, pix) in img.enumerate_pixels() {
                let Rgba([r, g, b, _]) = *pix;
                if (r as i32 - 220).abs() <= 20
                    && (g as i32 - 38).abs() <= 20
                    && (b as i32 - 38).abs() <= 20
                {
                    red[(y as usize / 50).min(11)] += 1;
                    redx[(x as usize / 50).min(15)] += 1;
                }
                // 地铁蓝路线（精确色值，无底图噪声）：①② 有 paths
                if (r as i32 - 0x1D).abs() <= 15
                    && (g as i32 - 0x4E).abs() <= 15
                    && (b as i32 - 0xD8).abs() <= 15
                {
                    bny = bny.min(y);
                    bxy = bxy.max(y);
                    b_n += 1;
                }
            }
            let report = red
                .iter()
                .enumerate()
                .filter(|(_, c)| **c > 0)
                .map(|(i, c)| format!("y{}..{}={}", i * 50, i * 50 + 49, c))
                .collect::<Vec<_>>()
                .join("  ");
            let blue = if b_n > 0 {
                format!(
                    "路线蓝 n={b_n} y∈[{bny},{bxy}]（tile13 预测 155..448，tile14 预测 10..595）"
                )
            } else {
                "无路线蓝".into()
            };
            println!("{tag}: 已存 {save}");
            println!("   红: {report}");
            if tag == "④仅markers" {
                let rx = redx
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| **c > 0)
                    .map(|(i, c)| format!("x{}..{}={}", i * 50, i * 50 + 49, c))
                    .collect::<Vec<_>>()
                    .join("  ");
                println!("   红x: {rx}（tile13 预测气球 x≈239/325/424/507）");
            }
            println!("   {blue}");
        }
        println!("（上/下边缘红像素只在带 labels 时出现 = labels 参数导致卡边）");
    }

    /// 诊断：标定高德静态地图的 zoom→比例函数。固定两个已知坐标的 marker
    /// （郑州站→河南博物院，Δlat=0.0425°），分别请求 zoom 11..15，
    /// 实测两 marker 质心的像素距离，与瓦片模型对照（tile z: 0.0368·2^z px，
    /// 若高德 zoom=z 对应 tile z+1 则数字翻倍）。
    /// 运行：`cargo test zoom_scale_ladder_probe -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn zoom_scale_ladder_probe() {
        let tool = GenerateCityMap::new(
            test_amap_key(),
            test_font_path(),
            "probe".into(),
            crate::tools::RateLimiter::new(400),
        );
        let (lon1, lat1) = (113.6416_f64, 34.7466_f64); // 郑州站（西南）
        let (lon2, lat2) = (113.6856_f64, 34.7891_f64); // 河南博物院（东北）
        for zoom in [11u8, 12, 13, 14] {
            let location = format!("{:.6},{:.6}", (lon1 + lon2) / 2.0, (lat1 + lat2) / 2.0);
            let markers = format!("mid,0xDC2626,:{lon1},{lat1};{lon2},{lat2}");
            let Ok(png) = tool
                .fetch_staticmap(&location, zoom, "", &markers, "")
                .await
            else {
                println!("zoom {zoom}: 请求失败");
                continue;
            };
            let img = image::load_from_memory(&png).expect("解码失败").to_rgba8();
            // 红像素按 x 中点分两簇，取各簇质心
            let (mut a_n, mut a_x, mut a_y, mut b_n, mut b_x, mut b_y) =
                (0u64, 0f64, 0f64, 0u64, 0f64, 0f64);
            let mid_x = img.width() as f64 / 2.0;
            for (x, y, p) in img.enumerate_pixels() {
                let Rgba([r, g, b, _]) = *p;
                if (r as i32 - 220).abs() <= 20
                    && (g as i32 - 38).abs() <= 20
                    && (b as i32 - 38).abs() <= 20
                {
                    let (xf, yf) = (x as f64, y as f64);
                    if xf < mid_x {
                        a_n += 1;
                        a_x += xf;
                        a_y += yf;
                    } else {
                        b_n += 1;
                        b_x += xf;
                        b_y += yf;
                    }
                }
            }
            if a_n == 0 || b_n == 0 {
                println!("zoom {zoom}: 红像素不足（a={a_n} b={b_n}）");
                continue;
            }
            let dx = (b_x / b_n as f64) - (a_x / a_n as f64);
            let dy = (b_y / b_n as f64) - (a_y / a_n as f64);
            let model_tile_z = 0.0368_f64 * 2f64.powi(zoom as i32); // 瓦片 zoom=z 的纵向像素距
            println!(
                "zoom {zoom}: 像素距 dx={dx:.0} dy={dy:.0}（tile z 模型预测 dy≈{model_tile_z:.0}；z+1 模型 {:.0}）",
                model_tile_z * 2.0
            );
        }
    }

    /// 诊断：解码城市图产物，实测 marker 红像素（DC2626，含底图自身红色元素噪声）
    /// 的纵向分布（50px 分带直方图）与图例框位置，验证 POI 是否真被图例盖住。
    /// 运行：`cargo test legend_occlusion_probe -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn legend_occlusion_probe() {
        for name in ["city_21.png", "city_21_c1.png"] {
            let path = std::path::Path::new("maps/test").join(name);
            let Ok(img) = image::open(&path) else {
                println!("{name}: 打不开，跳过");
                continue;
            };
            let img = img.to_rgba8();
            let (w, h) = img.dimensions();
            // 红像素分带直方图（50px 一带）
            let mut bands = [0u64; 12];
            let mut total = 0u64;
            for (_, y, p) in img.enumerate_pixels() {
                let Rgba([r, g, b, _]) = *p;
                if (r as i32 - 220).abs() <= 20
                    && (g as i32 - 38).abs() <= 20
                    && (b as i32 - 38).abs() <= 20
                {
                    bands[(y as usize / 50).min(11)] += 1;
                    total += 1;
                }
            }
            // 图例框顶边：底部 200px 内最长暗色横线（MARKER_RING 30,41,59，跨度 >3/4 宽）
            let mut legend_top = None;
            'outer: for y in (h.saturating_sub(200)..h).rev() {
                let mut run = 0u32;
                let mut best = 0u32;
                for x in 0..w {
                    let p = img.get_pixel(x, y);
                    let Rgba([r, g, b, _]) = *p;
                    if (r as i32 - 30).abs() <= 25
                        && (g as i32 - 41).abs() <= 25
                        && (b as i32 - 59).abs() <= 25
                    {
                        run += 1;
                        best = best.max(run);
                        if best > w * 3 / 4 {
                            legend_top = Some(y);
                            continue 'outer;
                        }
                    } else {
                        run = 0;
                    }
                }
            }
            println!("{name}: {w}×{h}，红像素总数 {total}");
            for (i, &c) in bands.iter().enumerate() {
                if c > 0 {
                    println!("  y {:3}..{:3}: {:5} █", i * 50, i * 50 + 49, c);
                }
            }
            println!("  图例顶边 y = {legend_top:?}");
        }
    }
}
