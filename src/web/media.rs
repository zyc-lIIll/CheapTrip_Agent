//! media：`/media/{kind}/{sid}/{*path}` —— 会话产物图片的静态下发。
//!
//! kind 白名单 `maps` / `hotels`，对应工作目录下同名目录；sid 走全局白名单校验，
//! 相对路径逐段校验（禁 `..`/`.`/隐藏文件/反斜杠）+ canonicalize 二次确认仍在
//! 基目录内，双保险防路径穿越。路由在鉴权层之内，口令不对拿不到图。

use std::path::{Path, PathBuf};

use axum::Extension;
use axum::extract::{Path as AxumPath, State};
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use tokio::fs;

use super::auth::repository::UserRecord;
use super::rest::{ApiError, bad_request, load_owned_session, not_found, valid_sid};
use super::state::SharedState;

/// 允许的产物目录（与 session.rs 删除联动、tools 落盘目录保持一致）。
const MEDIA_KINDS: [&str; 2] = ["maps", "hotels"];

/// GET /media/{kind}/{sid}/{*path}
pub async fn media_file(
    State(st): State<SharedState>,
    AxumPath((kind, sid, rest)): AxumPath<(String, String, String)>,
    owner: Option<Extension<UserRecord>>,
) -> Result<Response, ApiError> {
    if !MEDIA_KINDS.contains(&kind.as_str()) {
        return Err(not_found(format!("未知产物目录：{kind:?}")));
    }
    if !valid_sid(&sid) {
        return Err(bad_request(format!("非法会话 id：{sid:?}")));
    }
    if st.cfg.auth.enabled {
        let _ = load_owned_session(&st, &sid, owner.as_ref())?;
    }
    let rel = safe_rel_path(&rest).ok_or_else(|| bad_request("非法媒体路径"))?;

    let base = Path::new(kind.as_str());
    let base_canon = base
        .canonicalize()
        .map_err(|_| not_found("产物目录不存在"))?;
    let full = base.join(&sid).join(&rel);
    let canon = full.canonicalize().map_err(|_| not_found("文件不存在"))?;
    // canonicalize 已解析符号链接与 `..`：再确认确实在基目录内
    if !canon.starts_with(&base_canon) {
        return Err(not_found("文件不存在"));
    }

    let mime = mime_of(&canon);
    let bytes = fs::read(&canon).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            not_found("文件不存在")
        } else {
            ApiError::from(anyhow::anyhow!("读取 {} 失败：{e}", canon.display()))
        }
    })?;
    Ok(([(CONTENT_TYPE, mime)], bytes).into_response())
}

/// 相对路径白名单：按 `/` 逐段校验后重组，杜绝穿越与隐藏文件。
fn safe_rel_path(rest: &str) -> Option<PathBuf> {
    if rest.is_empty() || rest.len() > 1024 {
        return None;
    }
    let mut pb = PathBuf::new();
    let mut depth = 0usize;
    for seg in rest.split('/') {
        if seg.is_empty() || seg.starts_with('.') || seg.contains('\\') || seg.len() > 128 {
            return None;
        }
        pb.push(seg);
        depth += 1;
        if depth > 8 {
            return None;
        }
    }
    // 末段必须带扩展名（只服务产物文件，不列目录）
    let has_ext = pb.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        !e.is_empty() && e.len() <= 16 && e.bytes().all(|b| b.is_ascii_alphanumeric())
    });
    if !has_ext {
        return None;
    }
    Some(pb)
}

/// 只内联常见图片格式；其余一律 octet-stream（浏览器不会当图渲染）。
fn mime_of(p: &Path) -> &'static str {
    match p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::{AppState, router};
    use crate::{
        config::{Config, CostConfig, LlmConfig, SessionConfig, WebConfig},
        llm,
    };
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::sync::Arc;
    use tower::ServiceExt; // oneshot

    fn test_app() -> axum::Router {
        let cfg = Config {
            llm: LlmConfig {
                base_url: "http://127.0.0.1:9".into(),
                model: "test".into(),
                api_key_env: "CHEAPTRIP_TEST_KEY".into(),
                temperature: 0.7,
                max_tokens: 100,
                connect_timeout_secs: 1,
                read_timeout_secs: 1,
                provider: Default::default(),
                reasoning_effort: Default::default(),
            },
            cost: CostConfig {
                input_per_1m: 0.0,
                output_per_1m: 0.0,
            },
            session: SessionConfig {
                max_messages: None,
                max_sessions: None,
            },
            xhs: Default::default(),
            web: WebConfig::default(),
            auth: Default::default(),
        };
        let state = Arc::new(AppState::new(
            cfg,
            llm::Client::for_test(),
            Arc::new(|_sid: &str| -> Vec<Box<crate::tools::DynTool>> { Vec::new() }),
        ));
        router(state)
    }

    /// 1×1 PNG 的最小字节（合法 PNG 头，能过 mime/长度校验即可）。
    const PNG_1PX: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89,
    ];

    /// 正常取图 200 + mime；穿越/隐藏/未知目录/缺文件各自被拦。
    #[tokio::test]
    async fn media_serve_and_traversal() {
        let sid = "test-media";
        let dir = PathBuf::from("maps").join(sid);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("overview_1.png"), PNG_1PX).unwrap();

        let app = test_app();

        // 正常：200 + image/png
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/media/maps/{sid}/overview_1.png"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()["content-type"], "image/png");

        // 穿越尝试：编码 ../ 不会出现在 wildcard（matchit 按段匹配），但 %2E%2E 解码后进校验
        for uri in [
            format!("/media/maps/{sid}/..%2Foverview_1.png"),
            format!("/media/maps/{sid}/.%2E%2Foverview_1.png"),
            format!("/media/maps/{sid}/.hidden/overview_1.png"),
            format!("/media/maps/{sid}/sub/"), // 无文件名（末段无扩展名）
            format!("/media/unknown/{sid}/overview_1.png"), // 未知 kind
            format!("/media/maps/{sid}/no_such.png"), // 缺文件
            "/media/maps/..%2Fetc%2Fpasswd/x.png".to_string(), // sid 穿越
        ] {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(uri.clone())
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert!(
                res.status() == StatusCode::BAD_REQUEST || res.status() == StatusCode::NOT_FOUND,
                "uri={uri} 应被拒绝，实际 {}",
                res.status()
            );
        }

        // 清理
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// safe_rel_path 白名单规则本身。
    #[test]
    fn safe_rel_path_rules() {
        assert!(safe_rel_path("overview_1.png").is_some());
        assert!(safe_rel_path("hotel_2/review.jpg").is_some());
        assert!(safe_rel_path("../escape.png").is_none());
        assert!(safe_rel_path("a/../../b.png").is_none());
        assert!(safe_rel_path(".hidden/x.png").is_none());
        assert!(safe_rel_path("a//b.png").is_none());
        assert!(safe_rel_path("a\\b.png").is_none());
        assert!(safe_rel_path("nodir/").is_none()); // 末段无扩展名
        assert!(safe_rel_path("dir/noext").is_none());
        assert!(safe_rel_path("").is_none());
        assert!(safe_rel_path(&format!("{}/x.png", "d".repeat(300))).is_none());
    }
}
