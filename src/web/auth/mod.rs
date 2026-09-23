//! M2 认证基础设施。
//!
//! 提供 SQLite 初始化、数据模型、Argon2id 密码原语、账号 Session、CSRF、
//! 登录限速、强制改密和权限提取器；管理员用户管理 API 位于 `admin.rs`。

pub(crate) mod admin;
pub(crate) mod model;
pub(crate) mod password;
pub(crate) mod repository;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use tower_sessions::Session;

use crate::web::state::SharedState;

use std::{path::Path, str::FromStr};

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tower_sessions_sqlx_store::SqliteStore;

use crate::config::AuthConfig;

pub struct AuthBackend {
    pub pool: sqlx::SqlitePool,
    pub sessions: SqliteStore,
}

impl AuthBackend {
    pub async fn connect(cfg: &AuthConfig) -> Result<Self> {
        ensure_sqlite_parent(&cfg.database_url)?;
        let options = SqliteConnectOptions::from_str(&cfg.database_url)
            .context("解析 [auth].database_url 失败")?
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .context("连接认证 SQLite 数据库失败")?;
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .context("执行认证数据库迁移失败")?;

        let sessions = SqliteStore::new(pool.clone());
        sessions
            .migrate()
            .await
            .context("创建服务端 Session 表失败")?;
        Ok(Self { pool, sessions })
    }
}

fn ensure_sqlite_parent(database_url: &str) -> Result<()> {
    let Some(path) = database_url.strip_prefix("sqlite://") else {
        return Ok(());
    };
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let Some(parent) = Path::new(path).parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(parent)
        .with_context(|| format!("创建认证数据库目录 {} 失败", parent.display()))
}

const USER_ID: &str = "auth.user_id";
const USER_VERSION: &str = "auth.session_version";
const CSRF_TOKEN: &str = "auth.csrf_token";

#[derive(Debug, Deserialize)]
pub(crate) struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChangePasswordRequest {
    /// 首次登录（must_change_password=true）时可省略；普通改密必须提供。
    pub current_password: Option<String>,
    pub password: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct CurrentUser {
    pub id: i64,
    pub username: String,
    pub role: model::Role,
    pub must_change_password: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct AuthResponse {
    pub user: CurrentUser,
    pub csrf_token: String,
}

pub(crate) struct AuthError(StatusCode, &'static str);

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}

pub(crate) async fn login(
    State(state): State<SharedState>,
    session: Session,
    headers: HeaderMap,
    Json(input): Json<LoginRequest>,
) -> Result<Json<AuthResponse>, AuthError> {
    let Some(backend) = &state.auth else {
        return Err(AuthError(StatusCode::NOT_FOUND, "账号登录尚未启用"));
    };
    check_origin(&headers, &state.cfg.web.cors_origins)?;
    let user = repository::find_by_username(&backend.pool, &input.username)
        .await
        .map_err(|_| AuthError(StatusCode::INTERNAL_SERVER_ERROR, "登录服务暂时不可用"))?;
    let Some(user) = user else {
        return Err(AuthError(StatusCode::UNAUTHORIZED, "用户名或密码错误"));
    };
    if !user.enabled {
        return Err(AuthError(StatusCode::UNAUTHORIZED, "用户名或密码错误"));
    }
    if user.locked_until.is_some_and(|until| until > now_epoch()) {
        return Err(AuthError(
            StatusCode::TOO_MANY_REQUESTS,
            "登录尝试过于频繁，请稍后再试",
        ));
    }
    let password_ok = password::verify_password(&input.password, &user.password_hash)
        .map_err(|_| AuthError(StatusCode::UNAUTHORIZED, "用户名或密码错误"))?;
    if !password_ok {
        let locked = repository::record_login_failure(
            &backend.pool,
            user.id,
            state.cfg.auth.max_login_failures,
            state.cfg.auth.lockout_secs,
        )
        .await
        .map_err(|_| AuthError(StatusCode::INTERNAL_SERVER_ERROR, "登录服务暂时不可用"))?;
        return Err(if locked {
            AuthError(
                StatusCode::TOO_MANY_REQUESTS,
                "登录尝试过于频繁，请稍后再试",
            )
        } else {
            AuthError(StatusCode::UNAUTHORIZED, "用户名或密码错误")
        });
    }
    repository::mark_login(&backend.pool, user.id)
        .await
        .map_err(|_| AuthError(StatusCode::INTERNAL_SERVER_ERROR, "登录服务暂时不可用"))?;
    session
        .cycle_id()
        .await
        .map_err(|_| AuthError(StatusCode::INTERNAL_SERVER_ERROR, "登录服务暂时不可用"))?;
    session
        .insert(USER_ID, user.id)
        .await
        .map_err(|_| AuthError(StatusCode::INTERNAL_SERVER_ERROR, "登录服务暂时不可用"))?;
    session
        .insert(USER_VERSION, user.session_version)
        .await
        .map_err(|_| AuthError(StatusCode::INTERNAL_SERVER_ERROR, "登录服务暂时不可用"))?;
    let csrf_token = new_csrf_token();
    session
        .insert(CSRF_TOKEN, &csrf_token)
        .await
        .map_err(|_| AuthError(StatusCode::INTERNAL_SERVER_ERROR, "登录服务暂时不可用"))?;
    Ok(Json(AuthResponse {
        user: user.into(),
        csrf_token,
    }))
}

pub(crate) async fn logout(
    session: Session,
    headers: HeaderMap,
    State(state): State<SharedState>,
) -> Result<StatusCode, AuthError> {
    check_csrf(&headers, &state.cfg.web.cors_origins, &session).await?;
    session
        .flush()
        .await
        .map_err(|_| AuthError(StatusCode::INTERNAL_SERVER_ERROR, "退出登录失败"))?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn me(
    State(state): State<SharedState>,
    session: Session,
) -> Result<Json<AuthResponse>, AuthError> {
    let Some(backend) = &state.auth else {
        return Err(AuthError(StatusCode::NOT_FOUND, "账号登录尚未启用"));
    };
    let user_id = session
        .get::<i64>(USER_ID)
        .await
        .map_err(|_| AuthError(StatusCode::INTERNAL_SERVER_ERROR, "读取登录状态失败"))?;
    let Some(user_id) = user_id else {
        return Err(AuthError(StatusCode::UNAUTHORIZED, "尚未登录"));
    };
    let Some(user) = repository::find_by_id(&backend.pool, user_id)
        .await
        .map_err(|_| AuthError(StatusCode::INTERNAL_SERVER_ERROR, "读取登录状态失败"))?
    else {
        return Err(AuthError(StatusCode::UNAUTHORIZED, "登录状态已失效"));
    };
    let version = session
        .get::<i64>(USER_VERSION)
        .await
        .map_err(|_| AuthError(StatusCode::INTERNAL_SERVER_ERROR, "读取登录状态失败"))?;
    if !user.enabled || version != Some(user.session_version) {
        let _ = session.flush().await;
        return Err(AuthError(StatusCode::UNAUTHORIZED, "登录状态已失效"));
    }
    let csrf_token = ensure_csrf_token(&session).await?;
    Ok(Json(AuthResponse {
        user: user.into(),
        csrf_token,
    }))
}

pub(crate) async fn change_password(
    State(state): State<SharedState>,
    session: Session,
    headers: HeaderMap,
    Json(input): Json<ChangePasswordRequest>,
) -> Result<Json<AuthResponse>, AuthError> {
    let Some(backend) = &state.auth else {
        return Err(AuthError(StatusCode::NOT_FOUND, "账号登录尚未启用"));
    };
    check_csrf(&headers, &state.cfg.web.cors_origins, &session).await?;
    let user_id = session_user_id(&session).await?;
    let user = repository::find_by_id(&backend.pool, user_id)
        .await
        .map_err(|_| AuthError(StatusCode::INTERNAL_SERVER_ERROR, "读取用户失败"))?
        .ok_or(AuthError(StatusCode::UNAUTHORIZED, "登录状态已失效"))?;
    if !user.must_change_password {
        let Some(current_password) = input.current_password.as_deref() else {
            return Err(AuthError(StatusCode::BAD_REQUEST, "请输入当前密码"));
        };
        let current_ok = password::verify_password(current_password, &user.password_hash)
            .map_err(|_| AuthError(StatusCode::UNAUTHORIZED, "当前密码错误"))?;
        if !current_ok {
            return Err(AuthError(StatusCode::UNAUTHORIZED, "当前密码错误"));
        }
    }
    if input.password.chars().count() < 8 {
        return Err(AuthError(StatusCode::BAD_REQUEST, "密码至少需要 8 个字符"));
    }
    let hash = password::hash_password(&input.password)
        .map_err(|_| AuthError(StatusCode::INTERNAL_SERVER_ERROR, "生成密码哈希失败"))?;
    let version = repository::change_password(&backend.pool, user_id, &hash)
        .await
        .map_err(|_| AuthError(StatusCode::INTERNAL_SERVER_ERROR, "修改密码失败"))?;
    let target_id = user.id.to_string();
    repository::record_audit(
        &backend.pool,
        repository::AuditEntry {
            actor_user_id: Some(user.id),
            actor_username: Some(&user.username),
            action: "auth.change_password",
            target_type: Some("user"),
            target_id: Some(&target_id),
            target_label: Some(&user.username),
            result: "success",
            detail_json: None,
        },
    )
    .await
    .map_err(|_| AuthError(StatusCode::INTERNAL_SERVER_ERROR, "记录密码修改失败"))?;
    session
        .cycle_id()
        .await
        .map_err(|_| AuthError(StatusCode::INTERNAL_SERVER_ERROR, "修改密码失败"))?;
    session
        .insert(USER_VERSION, version)
        .await
        .map_err(|_| AuthError(StatusCode::INTERNAL_SERVER_ERROR, "修改密码失败"))?;
    let csrf_token = new_csrf_token();
    session
        .insert(CSRF_TOKEN, &csrf_token)
        .await
        .map_err(|_| AuthError(StatusCode::INTERNAL_SERVER_ERROR, "修改密码失败"))?;
    let user = repository::find_by_id(&backend.pool, user_id)
        .await
        .map_err(|_| AuthError(StatusCode::INTERNAL_SERVER_ERROR, "读取用户失败"))?
        .ok_or(AuthError(StatusCode::UNAUTHORIZED, "登录状态已失效"))?;
    Ok(Json(AuthResponse {
        user: user.into(),
        csrf_token,
    }))
}

pub(crate) async fn session_user(
    state: &SharedState,
    session: &Session,
) -> Result<Option<repository::UserRecord>> {
    let Some(backend) = &state.auth else {
        return Ok(None);
    };
    let Some(user_id) = session.get::<i64>(USER_ID).await? else {
        return Ok(None);
    };
    let Some(user) = repository::find_by_id(&backend.pool, user_id).await? else {
        return Ok(None);
    };
    let version = session.get::<i64>(USER_VERSION).await?;
    if !user.enabled || version != Some(user.session_version) {
        return Ok(None);
    }
    Ok(Some(user))
}

async fn session_user_id(session: &Session) -> Result<i64, AuthError> {
    session
        .get::<i64>(USER_ID)
        .await
        .map_err(|_| AuthError(StatusCode::UNAUTHORIZED, "尚未登录"))?
        .ok_or(AuthError(StatusCode::UNAUTHORIZED, "尚未登录"))
}

async fn ensure_csrf_token(session: &Session) -> Result<String, AuthError> {
    if let Some(token) = session
        .get::<String>(CSRF_TOKEN)
        .await
        .map_err(|_| AuthError(StatusCode::INTERNAL_SERVER_ERROR, "读取安全令牌失败"))?
    {
        return Ok(token);
    }
    let token = new_csrf_token();
    session
        .insert(CSRF_TOKEN, &token)
        .await
        .map_err(|_| AuthError(StatusCode::INTERNAL_SERVER_ERROR, "生成安全令牌失败"))?;
    Ok(token)
}

fn new_csrf_token() -> String {
    tower_sessions::session::Id::default().to_string()
}

async fn check_csrf(
    headers: &HeaderMap,
    origins: &[String],
    session: &Session,
) -> Result<(), AuthError> {
    check_origin(headers, origins)?;
    let expected = session
        .get::<String>(CSRF_TOKEN)
        .await
        .map_err(|_| AuthError(StatusCode::FORBIDDEN, "CSRF 校验失败"))?;
    let supplied = headers.get("x-csrf-token").and_then(|v| v.to_str().ok());
    if expected
        .as_deref()
        .is_none_or(|token| Some(token) != supplied)
    {
        return Err(AuthError(StatusCode::FORBIDDEN, "CSRF 校验失败"));
    }
    Ok(())
}

pub(crate) async fn check_request_csrf(
    headers: HeaderMap,
    session: &Session,
    origins: &[String],
) -> Result<(), AuthError> {
    check_csrf(&headers, origins, session).await
}

fn check_origin(headers: &HeaderMap, origins: &[String]) -> Result<(), AuthError> {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return Ok(());
    };
    if origin_allowed(origin, headers, origins) {
        Ok(())
    } else {
        Err(AuthError(StatusCode::FORBIDDEN, "请求来源未被允许"))
    }
}

/// 判断浏览器来源是否允许。
///
/// 显式配置的来源用于真正的跨域前端；同源页面则依据 Host 自动放行，
/// 因此不需要把后端监听端口再手工写进 `cors_origins`。反向代理部署时，
/// 使用标准的 `X-Forwarded-Proto` 判断原始请求协议（未设置时按本机 HTTP）。
pub(crate) fn origin_allowed(
    origin: &HeaderValue,
    headers: &HeaderMap,
    origins: &[String],
) -> bool {
    let Ok(origin_text) = origin.to_str() else {
        return false;
    };
    if origins.iter().any(|allowed| allowed == origin_text) {
        return true;
    }
    same_origin(origin_text, headers)
}

fn same_origin(origin: &str, headers: &HeaderMap) -> bool {
    let Some((scheme, remainder)) = origin.split_once("://") else {
        return false;
    };
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        return false;
    }
    let authority = remainder.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') {
        return false;
    }
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let forwarded_proto = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("http");
    scheme.eq_ignore_ascii_case(forwarded_proto) && authority.eq_ignore_ascii_case(host.trim())
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

impl From<repository::UserRecord> for CurrentUser {
    fn from(user: repository::UserRecord) -> Self {
        Self {
            id: user.id,
            username: user.username,
            role: user.role,
            must_change_password: user.must_change_password,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, CostConfig, LlmConfig, SessionConfig, WebConfig};
    use crate::llm;
    use crate::web::state::AppState;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use std::sync::Arc;
    use tower::ServiceExt;

    #[tokio::test]
    async fn sqlite_auth_schema_migrates() -> Result<()> {
        let cfg = AuthConfig {
            enabled: true,
            database_url: "sqlite::memory:".into(),
            ..Default::default()
        };
        let backend = AuthBackend::connect(&cfg).await?;
        let user_table: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'users'",
        )
        .fetch_one(&backend.pool)
        .await?;
        assert_eq!(user_table, 1);
        Ok(())
    }

    #[test]
    fn sqlite_parent_directory_is_created() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "cheaptrip-auth-parent-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("unnamed")
        ));
        let database = root.join("nested").join("auth.db");
        ensure_sqlite_parent(&format!("sqlite://{}?mode=rwc", database.display()))?;
        assert!(root.join("nested").is_dir());
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[tokio::test]
    async fn auth_login_me_logout_roundtrip() -> Result<()> {
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
            auth: AuthConfig {
                enabled: true,
                database_url: "sqlite::memory:".into(),
                ..Default::default()
            },
        };
        let backend = Arc::new(AuthBackend::connect(&cfg.auth).await?);
        let hash = password::hash_password("correct horse battery staple")?;
        repository::create_user(&backend.pool, "admin", &hash, model::Role::Admin).await?;
        let state = Arc::new(AppState::new_with_auth(
            cfg,
            llm::Client::for_test(),
            Arc::new(|_| Vec::new()),
            Some(backend),
        ));
        let app = crate::web::router(state);
        let admin_without_login = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/admin/users")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(admin_without_login.status(), StatusCode::UNAUTHORIZED);
        let login = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/login")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"username":"admin","password":"correct horse battery staple"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::OK);
        let cookie = login
            .headers()
            .get(header::SET_COOKIE)
            .context("登录应下发 Session Cookie")?
            .to_str()?
            .split(';')
            .next()
            .context("Set-Cookie 为空")?
            .to_owned();
        assert!(
            login.headers()[header::SET_COOKIE]
                .to_str()?
                .contains("HttpOnly")
        );
        let login_body = axum::body::to_bytes(login.into_body(), usize::MAX).await?;
        let auth_response: AuthResponse = serde_json::from_slice(&login_body)?;
        assert!(!auth_response.csrf_token.is_empty());

        let me = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/auth/me")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(me.status(), StatusCode::OK);

        let protected = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/meta")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(protected.status(), StatusCode::FORBIDDEN);

        let missing_csrf = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/logout")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing_csrf.status(), StatusCode::FORBIDDEN);

        let logout = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/logout")
                    .header(header::COOKIE, &cookie)
                    .header("x-csrf-token", &auth_response.csrf_token)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(logout.status(), StatusCode::NO_CONTENT);
        let me_after = app
            .oneshot(
                Request::builder()
                    .uri("/api/auth/me")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(me_after.status(), StatusCode::UNAUTHORIZED);
        Ok(())
    }

    #[tokio::test]
    async fn regular_user_cannot_access_admin_settings_or_integrations() -> Result<()> {
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
            auth: AuthConfig {
                enabled: true,
                database_url: "sqlite::memory:".into(),
                ..Default::default()
            },
        };
        let backend = Arc::new(AuthBackend::connect(&cfg.auth).await?);
        let hash = password::hash_password("user password")?;
        repository::create_user(&backend.pool, "ordinary", &hash, model::Role::User).await?;
        let state = Arc::new(AppState::new_with_auth(
            cfg,
            llm::Client::for_test(),
            Arc::new(|_| Vec::new()),
            Some(backend),
        ));
        let app = crate::web::router(state);
        let login = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/login")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"username":"ordinary","password":"user password"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::OK);
        let cookie = login
            .headers()
            .get(header::SET_COOKIE)
            .context("普通用户登录应下发 Session Cookie")?
            .to_str()?
            .split(';')
            .next()
            .context("Set-Cookie 为空")?
            .to_owned();

        for path in ["/api/admin/settings", "/api/admin/integrations"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header(header::COOKIE, &cookie)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
        }
        Ok(())
    }

    #[test]
    fn origin_allowlist_accepts_same_origin_and_proxy_scheme() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("app.example.com"));
        assert!(origin_allowed(
            &HeaderValue::from_static("http://app.example.com"),
            &headers,
            &[]
        ));
        assert!(!origin_allowed(
            &HeaderValue::from_static("http://evil.example.com"),
            &headers,
            &[]
        ));

        headers.insert("x-forwarded-proto", HeaderValue::from_static("https, http"));
        assert!(origin_allowed(
            &HeaderValue::from_static("https://app.example.com"),
            &headers,
            &[]
        ));
        assert!(!origin_allowed(
            &HeaderValue::from_static("http://app.example.com"),
            &headers,
            &[]
        ));
        assert!(origin_allowed(
            &HeaderValue::from_static("https://frontend.example.com"),
            &headers,
            &["https://frontend.example.com".to_owned()]
        ));
    }
}
