//! Web 前端服务层（M0 已落地）：axum 包 Core，前后端分离 API 基座。
//!
//! 分层：`mod.rs` 路由/鉴权/CORS/冒烟页 + `rest.rs` 会话 CRUD +
//! `state.rs` 共享状态（会话→Core 句柄/事件扇出/并发闸门）+
//! `ws.rs` WS 推流（chat/stop）+ `media.rs` 产物图片下发 + React 静态产物托管。
//!
//! 迁移纪律（web-plan 原则 2/3）：URL 与跨域来源走 config.toml [web]，口令走 .env，
//! 同源来源按请求 Host 自动识别；代码不硬编码任何域名。API key 与访问口令只在本进程，永不下发前端。

mod integrations;
mod media;
mod rest;
mod state;
mod ws;
// M2 骨架已编译进来，但 enabled=false 且认证/设置路由尚未挂载。
#[allow(dead_code)]
pub(crate) mod auth;
#[allow(dead_code)]
mod settings;

use anyhow::{Context, Result};
use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use time::Duration;
use tokio::net::TcpListener;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::services::{ServeDir, ServeFile};
use tower_sessions::{Expiry, SessionManagerLayer};

use crate::config::Config;
use crate::llm;
use crate::tools::DynTool;
use auth::model::Role;
use auth::repository::UserRecord;
use state::AppState;

/// 工具构建函数（main.rs 提供，按 session_id 组装工具列表）。
pub type BuildTools = dyn Fn(&str) -> Vec<Box<DynTool>> + Send + Sync;

/// Web 服务入口：`cargo run -- web`。
pub async fn run(mut cfg: Config, client: llm::Client, build_tools: Arc<BuildTools>) -> Result<()> {
    let bind = cfg.web.bind.clone();
    if cfg.web.token.trim().is_empty() {
        tracing::warn!(
            "[web] 未设置访问口令（{}）——接口无鉴权，仅限本机使用；公网暴露前必须设置",
            cfg.web.token_env
        );
    }
    let auth = if cfg.auth.enabled {
        Some(Arc::new(auth::AuthBackend::connect(&cfg.auth).await?))
    } else {
        None
    };
    if let Some(backend) = &auth {
        settings::apply_overrides(&mut cfg, &backend.pool).await?;
    }
    // 管理员设置可能覆盖 LLM 端点/超时；认证关闭时沿用 main 已构建的客户端。
    let client = if auth.is_some() {
        llm::Client::new(&cfg)?
    } else {
        client
    };
    let state = Arc::new(AppState::new_with_auth(cfg, client, build_tools, auth));
    let app = router(state);

    let listener = TcpListener::bind(&bind)
        .await
        .with_context(|| format!("[web] 绑定 {bind} 失败"))?;
    tracing::info!("[web] listening on http://{bind}");
    axum::serve(listener, app)
        .await
        .context("[web] 服务异常退出")
}

/// 路由装配（run 与测试共用）。
fn router(state: Arc<AppState>) -> Router {
    let cors_origins = state.cfg.web.cors_origins.clone();
    let static_dir = state.cfg.web.static_dir.clone();
    // 静态壳不含密钥，公开以便浏览器加载 /assets；实际 API/WS/media 仍须带 WEB_TOKEN。
    let public: Router<Arc<AppState>> = if Path::new(&static_dir).join("index.html").is_file() {
        Router::new().fallback_service(
            ServeDir::new(&static_dir)
                .not_found_service(ServeFile::new(Path::new(&static_dir).join("index.html"))),
        )
    } else {
        tracing::warn!(
            "[web] 未找到 {static_dir}/index.html，暂时使用 M0 冒烟页；请先在 web/ 运行 npm run build"
        );
        Router::new().fallback(get(test_page))
    };

    let protected: Router<Arc<AppState>> = Router::new()
        .route("/api/meta", get(rest::meta))
        .route(
            "/api/sessions",
            get(rest::list_sessions).post(rest::create_session),
        )
        .route(
            "/api/sessions/{sid}",
            get(rest::session_detail).delete(rest::delete_session),
        )
        .route("/api/sessions/{sid}/rename", post(rest::rename_session))
        .route(
            "/api/sessions/{sid}/mode",
            axum::routing::put(rest::update_mode),
        )
        .route("/api/sessions/{sid}/notes", get(rest::session_notes))
        .route("/ws/{sid}", get(ws::ws_handler))
        .route("/media/{kind}/{sid}/{*path}", get(media::media_file));
    let protected = if state.cfg.auth.enabled {
        protected.layer(middleware::from_fn_with_state(state.clone(), account_auth))
    } else {
        protected.layer(middleware::from_fn_with_state(state.clone(), token_auth))
    };

    // CORS 在最外层，保证无口令 OPTIONS 也能通过；静态资源不进 auth。
    let mut app = public.merge(protected);
    if state.auth.is_some() {
        app = app.merge(
            Router::new()
                .route("/api/auth/login", post(auth::login))
                .route("/api/auth/logout", post(auth::logout))
                .route("/api/auth/me", get(auth::me))
                .route("/api/auth/change-password", post(auth::change_password))
                .with_state(state.clone()),
        );
        let admin = Router::new()
            .route(
                "/api/admin/users",
                get(auth::admin::list_users).post(auth::admin::create_user),
            )
            .route("/api/admin/users/{id}", patch(auth::admin::update_user))
            .route(
                "/api/admin/users/{id}/reset-password",
                post(auth::admin::reset_password),
            )
            .route(
                "/api/admin/users/{id}/revoke-sessions",
                post(auth::admin::revoke_sessions),
            )
            .route(
                "/api/admin/users/{id}/delete",
                post(auth::admin::delete_user),
            )
            .layer(middleware::from_fn(admin_only))
            .layer(middleware::from_fn_with_state(state.clone(), account_auth))
            .with_state(state.clone());
        app = app.merge(admin);
        let settings = Router::new()
            .route("/api/admin/settings", get(settings::get_settings))
            .route(
                "/api/admin/settings/model",
                axum::routing::put(settings::update_model),
            )
            .route("/api/admin/settings/secrets", get(settings::get_secrets))
            .layer(middleware::from_fn(admin_only))
            .layer(middleware::from_fn_with_state(state.clone(), account_auth))
            .with_state(state.clone());
        app = app.merge(settings);
        let integrations = Router::new()
            .route("/api/admin/integrations", get(integrations::list))
            .route(
                "/api/admin/integrations/{kind}/enabled",
                axum::routing::put(integrations::set_enabled),
            )
            .route(
                "/api/admin/integrations/xhs/login/start",
                post(integrations::start_xhs_login),
            )
            .route(
                "/api/admin/integrations/xhs/login/{task_id}",
                get(integrations::xhs_login_status),
            )
            .route(
                "/api/admin/integrations/xhs/login/{task_id}/qr",
                get(integrations::xhs_login_qr),
            )
            .route(
                "/api/admin/integrations/xhs/logout",
                post(integrations::logout_xhs),
            )
            .layer(middleware::from_fn(admin_only))
            .layer(middleware::from_fn_with_state(state.clone(), account_auth))
            .with_state(state.clone());
        app = app.merge(integrations);
    }
    let app = app
        .layer(cors_layer(&cors_origins))
        .with_state(state.clone());
    if let Some(auth) = &state.auth {
        let session_layer = SessionManagerLayer::new(auth.sessions.clone())
            .with_name(state.cfg.auth.cookie_name.clone())
            .with_http_only(true)
            .with_same_site(tower_sessions::cookie::SameSite::Lax)
            .with_secure(state.cfg.auth.secure_cookie)
            .with_expiry(Expiry::OnInactivity(Duration::hours(
                state.cfg.auth.session_ttl_hours as i64,
            )));
        app.layer(session_layer)
    } else {
        app
    }
}

/// 构建产物缺失时的 M0 兜底页，方便 API/WS 冒烟及不安装 Node 的开发环境。
async fn test_page() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("test_page.html"),
    )
}

/// 认证关闭时使用简单口令鉴权；认证开启时改用账号 Session。
async fn token_auth(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let expected = state.cfg.web.token.trim();
    if expected.is_empty() {
        return next.run(req).await;
    }
    let via_query = query_token(req.uri().query()).is_some_and(|t| t == expected);
    let via_header = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| t == expected);
    if via_query || via_header {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            "缺少或错误的访问口令（?token=xxx 或 Authorization: Bearer xxx）",
        )
            .into_response()
    }
}

async fn account_auth(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Response {
    let Some(session) = req.extensions().get::<tower_sessions::Session>().cloned() else {
        return (StatusCode::UNAUTHORIZED, "尚未登录").into_response();
    };
    match auth::session_user(&state, &session).await {
        Ok(Some(user)) => {
            if user.must_change_password {
                return (
                    StatusCode::FORBIDDEN,
                    "首次登录必须先修改密码（POST /api/auth/change-password）",
                )
                    .into_response();
            }
            if req.method() != axum::http::Method::GET && req.method() != axum::http::Method::HEAD {
                let headers = req.headers().clone();
                if let Err(error) =
                    auth::check_request_csrf(headers, &session, &state.cfg.web.cors_origins).await
                {
                    return error.into_response();
                }
            }
            req.extensions_mut().insert(user);
            next.run(req).await
        }
        Ok(None) => (StatusCode::UNAUTHORIZED, "尚未登录").into_response(),
        Err(error) => {
            tracing::error!(error = %error, "读取账号 Session 失败");
            (StatusCode::INTERNAL_SERVER_ERROR, "认证服务暂时不可用").into_response()
        }
    }
}

/// 管理员路由的第二层权限校验；即使未来路由装配变化，也不会把普通用户放进管理 API。
async fn admin_only(req: Request, next: Next) -> Response {
    match req.extensions().get::<UserRecord>() {
        Some(user) if user.role == Role::Admin => next.run(req).await,
        Some(_) => (StatusCode::FORBIDDEN, "仅管理员可用").into_response(),
        None => (StatusCode::UNAUTHORIZED, "尚未登录").into_response(),
    }
}

/// 从 query string 里取 token 参数值（不引入 URL 解码：口令约定只用安全字符）。
fn query_token(query: Option<&str>) -> Option<&str> {
    query?.split('&').find_map(|kv| {
        kv.split_once('=')
            .filter(|(k, _)| *k == "token")
            .map(|(_, v)| v)
    })
}

/// CORS 来源策略：显式列表放行跨域前端，同源页面依据 Host 自动放行。
fn cors_layer(origins: &[String]) -> CorsLayer {
    let configured = Arc::new(origins.to_vec());
    let origins_for_predicate = configured.clone();
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(move |origin, parts| {
            auth::origin_allowed(origin, &parts.headers, origins_for_predicate.as_slice())
        }))
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::HEAD,
            Method::OPTIONS,
        ])
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            header::ORIGIN,
            header::HeaderName::from_static("x-csrf-token"),
        ])
        .allow_credentials(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, CostConfig, LlmConfig, SessionConfig, WebConfig};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt; // oneshot

    fn test_cfg(token: &str) -> Config {
        Config {
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
            web: WebConfig {
                token: token.into(),
                ..Default::default()
            },
            auth: Default::default(),
        }
    }

    fn test_app(token: &str) -> Router {
        let state = Arc::new(AppState::new(
            test_cfg(token),
            llm::Client::for_test(),
            Arc::new(|_sid: &str| -> Vec<Box<DynTool>> { Vec::new() }),
        ));
        router(state)
    }

    /// 口令鉴权：无 token 401；?token= 放行；错误 token 401。
    #[tokio::test]
    async fn auth_token_required() {
        let app = test_app("sekrit");
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/sessions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/sessions?token=sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/sessions?token=wrong")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    /// token 未配置时全部放行（本机开发模式）。
    #[tokio::test]
    async fn auth_open_when_unset() {
        let app = test_app("");
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/sessions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    /// 正式静态壳必须能加载其 /assets；口令只保护会消耗模型额度的后端接口。
    #[tokio::test]
    async fn static_shell_is_public_while_api_stays_protected() {
        let app = test_app("sekrit");
        let shell = app
            .clone()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(shell.status(), StatusCode::OK);

        let api = app
            .oneshot(
                Request::builder()
                    .uri("/api/sessions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(api.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn cors_allows_same_origin_without_listing_backend_port() {
        let mut cfg = test_cfg("");
        cfg.web.cors_origins.clear();
        let state = Arc::new(AppState::new(
            cfg,
            llm::Client::for_test(),
            Arc::new(|_sid: &str| -> Vec<Box<DynTool>> { Vec::new() }),
        ));
        let app = router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(header::ORIGIN, "http://127.0.0.1:8080")
                    .header(header::HOST, "127.0.0.1:8080")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|value| value.to_str().ok()),
            Some("http://127.0.0.1:8080")
        );
    }
}
