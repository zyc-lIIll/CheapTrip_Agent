//! REST：会话 CRUD + 记忆笔记读取（web-plan §1.3 的 HTTP 包装）。
//!
//! 会话增删改查直接复用 session.rs 已有逻辑；删除联动 maps/hotels/notes 由
//! `Session::delete` 保证。所有路由过 mod.rs 的口令鉴权 + CORS。

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use axum::Extension;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use super::state::SharedState;
use crate::llm::{Message, Usage};
use crate::session::{self, QuickMode, Session, SessionMeta, WorkflowMode};
use crate::web::auth::repository::UserRecord;

// ---- DTO ----

/// 会话列表项（GET /api/sessions）。
#[derive(Serialize)]
pub struct SessionMetaDto {
    pub id: String,
    pub name: Option<String>,
    pub messages: usize,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    /// 最后对话时间（epoch 秒；来自 session.updated_at，不受文件 mtime 影响）
    pub updated_at: Option<u64>,
    pub workflow_mode: WorkflowMode,
    pub quick_mode: QuickMode,
}

impl From<&SessionMeta> for SessionMetaDto {
    fn from(m: &SessionMeta) -> Self {
        Self {
            id: m.id.clone(),
            name: m.name.clone(),
            messages: m.messages,
            prompt_tokens: m.prompt_tokens,
            completion_tokens: m.completion_tokens,
            total_tokens: m.total_tokens,
            updated_at: epoch_secs(m.updated_at),
            workflow_mode: m.workflow_mode,
            quick_mode: m.quick_mode,
        }
    }
}

/// 单会话详情（GET/POST/重命名后的返回体）：历史 + 阶段 + 用量。
#[derive(Serialize)]
pub struct SessionDetailDto {
    pub id: String,
    pub name: Option<String>,
    pub phase: u8,
    pub workflow_mode: WorkflowMode,
    pub quick_mode: QuickMode,
    pub usage: Usage,
    pub messages: Vec<Message>,
    pub updated_at: Option<u64>,
}

impl From<Session> for SessionDetailDto {
    fn from(s: Session) -> Self {
        Self {
            id: s.id,
            name: s.name,
            phase: s.phase,
            workflow_mode: s.workflow_mode,
            quick_mode: s.quick_mode,
            usage: s.usage,
            messages: s.messages,
            updated_at: epoch_secs(s.updated_at),
        }
    }
}

/// 记忆笔记（GET /api/sessions/{sid}/notes）；content 为 None 表示还没有笔记。
#[derive(Serialize)]
pub struct NotesDto {
    pub content: Option<String>,
}

/// Web 展示需要的非敏感运行信息；禁止扩展为整个 Config 的序列化。
#[derive(Serialize)]
pub struct MetaDto {
    pub model: String,
    pub input_per_1m: f64,
    pub output_per_1m: f64,
}

#[derive(Deserialize)]
pub struct CreateReq {
    pub name: Option<String>,
    #[serde(default)]
    pub workflow_mode: Option<WorkflowMode>,
}

#[derive(Deserialize)]
pub struct RenameReq {
    pub name: String,
}

// ---- 错误 ----

/// 统一错误响应：anyhow 链格式化进 body，状态码显式携带。
pub struct ApiError {
    pub status: StatusCode,
    pub msg: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, self.msg).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            msg: format!("{e:#}"),
        }
    }
}

pub(crate) fn bad_request(msg: impl Into<String>) -> ApiError {
    ApiError {
        status: StatusCode::BAD_REQUEST,
        msg: msg.into(),
    }
}

pub(crate) fn not_found(msg: impl Into<String>) -> ApiError {
    ApiError {
        status: StatusCode::NOT_FOUND,
        msg: msg.into(),
    }
}

// ---- handlers ----

/// GET /api/meta —— 模型名与计费单价；不包含端点、环境变量名或任何密钥。
pub async fn meta(State(st): State<SharedState>) -> Json<MetaDto> {
    Json(MetaDto {
        model: st.cfg.llm.model.clone(),
        input_per_1m: st.cfg.cost.input_per_1m,
        output_per_1m: st.cfg.cost.output_per_1m,
    })
}

/// GET /api/sessions —— 会话列表（按最后对话时间倒序；cli 临时会话不显示）。
pub async fn list_sessions(
    State(st): State<SharedState>,
    owner: Option<Extension<UserRecord>>,
) -> Result<Json<Vec<SessionMetaDto>>, ApiError> {
    let owner_id = auth_owner_id(&st, owner.as_ref())?;
    let metas = Session::list()?;
    let metas = if st.cfg.auth.enabled {
        metas
            .into_iter()
            .filter(|meta| meta.owner_user_id == Some(owner_id))
            .collect()
    } else {
        metas
    };
    Ok(Json(metas.iter().map(SessionMetaDto::from).collect()))
}

/// POST /api/sessions —— 新建会话（可选 name/workflow_mode），返回含开场白的详情。
pub async fn create_session(
    State(_st): State<SharedState>,
    owner: Option<Extension<UserRecord>>,
    body: Option<Json<CreateReq>>,
) -> Result<Json<SessionDetailDto>, ApiError> {
    let (name, workflow_mode) = body
        .map(|Json(r)| {
            (
                r.name
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
                r.workflow_mode.unwrap_or(WorkflowMode::Quick),
            )
        })
        .unwrap_or((None, WorkflowMode::Quick));
    let sid = new_session_id();
    let mut s = Session::new_with_workflow_mode(sid, workflow_mode);
    s.owner_user_id = owner.map(|Extension(user)| user.id);
    s.name = name;
    s.save().context("保存新会话失败")?;
    Ok(Json(s.into()))
}

/// GET /api/sessions/{sid} —— 历史 + phase + usage。
pub async fn session_detail(
    State(st): State<SharedState>,
    Path(sid): Path<String>,
    owner: Option<Extension<UserRecord>>,
) -> Result<Json<SessionDetailDto>, ApiError> {
    Ok(Json(load_owned_session(&st, &sid, owner.as_ref())?.into()))
}

/// DELETE /api/sessions/{sid} —— 删除会话（联动 maps/hotels/notes）。
pub async fn delete_session(
    State(st): State<SharedState>,
    Path(sid): Path<String>,
    owner: Option<Extension<UserRecord>>,
) -> Result<StatusCode, ApiError> {
    let _ = load_owned_session(&st, &sid, owner.as_ref())?;
    st.delete_session(&sid).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/sessions/{sid}/rename —— 重命名。
pub async fn rename_session(
    State(st): State<SharedState>,
    Path(sid): Path<String>,
    owner: Option<Extension<UserRecord>>,
    Json(req): Json<RenameReq>,
) -> Result<Json<SessionDetailDto>, ApiError> {
    let _ = load_owned_session(&st, &sid, owner.as_ref())?;
    let name = req.name.trim();
    if name.is_empty() {
        return Err(bad_request("name 不能为空"));
    }
    Session::rename(&sid, name.to_string()).context("重命名失败")?;
    Ok(Json(Session::load(&sid)?.into()))
}

/// PUT /api/sessions/{sid}/mode —— 顶层模式只允许在创建时选择，保留稳定拒绝响应。
pub async fn update_mode() -> Result<Json<SessionDetailDto>, ApiError> {
    // 废弃接口不读取 sid、所有权或忙碌状态，避免泄露任何会话是否存在。
    Err(ApiError {
        status: StatusCode::METHOD_NOT_ALLOWED,
        msg: "工作流模式只能在创建会话时选择，创建后不可修改".into(),
    })
}

/// GET /api/sessions/{sid}/notes —— 记忆笔记 markdown（右侧记忆栏数据源）。
pub async fn session_notes(
    State(st): State<SharedState>,
    Path(sid): Path<String>,
    owner: Option<Extension<UserRecord>>,
) -> Result<Json<NotesDto>, ApiError> {
    let _ = load_owned_session(&st, &sid, owner.as_ref())?;
    Ok(Json(NotesDto {
        content: session::load_notes(&sid)?,
    }))
}

// ---- helpers ----

/// 会话 id 白名单：ASCII 字母数字 + `-_`，长度上限 64。
/// REST/WS/media 共用（media 落地时同样校验），杜绝 `../` 路径穿越。
pub(crate) fn valid_sid(sid: &str) -> bool {
    !sid.is_empty()
        && sid.len() <= 64
        && sid
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn ensure_valid_sid(sid: &str) -> Result<(), ApiError> {
    if valid_sid(sid) {
        Ok(())
    } else {
        Err(bad_request(format!("非法会话 id：{sid:?}")))
    }
}

/// 认证开启时要求当前账号存在；认证关闭时返回哨兵值，仅用于保持列表接口的统一形状。
fn auth_owner_id(st: &SharedState, owner: Option<&Extension<UserRecord>>) -> Result<i64, ApiError> {
    if !st.cfg.auth.enabled {
        return Ok(0);
    }
    owner
        .map(|Extension(user)| user.id)
        .ok_or_else(|| ApiError {
            status: StatusCode::UNAUTHORIZED,
            msg: "尚未登录".into(),
        })
}

/// 加载并校验会话归属。认证开启时，不存在或不属于当前用户统一返回 404，
/// 避免通过响应差异枚举其他账号的会话 id。
pub(crate) fn load_owned_session(
    st: &SharedState,
    sid: &str,
    owner: Option<&Extension<UserRecord>>,
) -> Result<Session, ApiError> {
    ensure_valid_sid(sid)?;
    let session = Session::load(sid).map_err(|_| not_found(format!("会话 {sid} 不存在")))?;
    if st.cfg.auth.enabled {
        let owner_id = auth_owner_id(st, owner)?;
        if session.owner_user_id != Some(owner_id) {
            return Err(not_found(format!("会话 {sid} 不存在")));
        }
    }
    Ok(session)
}

/// 新会话 id：使用密码学随机的 URL-safe Session ID，避免可猜测会话路径。
fn new_session_id() -> String {
    format!("s{}", tower_sessions::session::Id::default())
}

fn epoch_secs(t: Option<SystemTime>) -> Option<u64> {
    t.and_then(|t| t.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, CostConfig, LlmConfig, SessionConfig, WebConfig};
    use crate::llm;
    use crate::web::auth::model::Role;
    use crate::web::{AppState, router};
    use axum::body::Body;
    use axum::http::Request;
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
        let state: SharedState = Arc::new(AppState::new(
            cfg,
            llm::Client::for_test(),
            Arc::new(|_sid: &str| -> Vec<Box<crate::tools::DynTool>> { Vec::new() }),
        ));
        router(state)
    }

    async fn json_body(res: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// REST 全链路：创建 → 详情 → 重命名 → 笔记（空）→ 删除 → 404。
    #[tokio::test]
    async fn rest_crud_roundtrip() {
        let app = test_app();

        // 创建（带名字）
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"name":"测试会话"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let dto = json_body(res).await;
        let sid = dto["id"].as_str().unwrap().to_string();
        assert_eq!(dto["name"], "测试会话");
        assert_eq!(dto["phase"], 0);
        assert_eq!(dto["workflow_mode"], "quick");
        assert_eq!(dto["quick_mode"], "auto");
        // 新会话带开场白
        assert_eq!(dto["messages"].as_array().unwrap().len(), 1);
        assert_eq!(dto["messages"][0]["role"], "assistant");

        // 创建时选择完整攻略并持久化选择。
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"workflow_mode":"full"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let full_dto = json_body(res).await;
        let full_sid = full_dto["id"].as_str().unwrap().to_string();
        assert_eq!(full_dto["workflow_mode"], "full");
        assert_eq!(full_dto["quick_mode"], "auto");

        // 创建后不再暴露模式修改路由。
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/api/sessions/{sid}/mode"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"workflow_mode":"full"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);

        // 详情
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/sessions/{sid}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let dto = json_body(res).await;
        assert_eq!(dto["id"], sid.as_str());
        assert_eq!(dto["workflow_mode"], "quick");
        assert_eq!(dto["quick_mode"], "auto");

        // 重命名
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/sessions/{sid}/rename"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"name":" 改名了 "}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let dto = json_body(res).await;
        assert_eq!(dto["name"], "改名了"); // trim 生效

        // 笔记：无笔记 → null
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/sessions/{sid}/notes"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let dto = json_body(res).await;
        assert!(dto["content"].is_null());

        // 删除 → 204；再取详情 → 404
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/sessions/{sid}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/sessions/{sid}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/sessions/{full_sid}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
    }

    /// 非法 sid（路径穿越/非白名单字符）一律 400，不触碰文件系统。
    /// 浏览器实际发送的是百分号编码形态，Path 解码后才进白名单校验。
    #[tokio::test]
    async fn invalid_sid_rejected() {
        let app = test_app();
        for uri in [
            "/api/sessions/..%2F..%2Fetc",
            "/api/sessions/%E4%B8%AD%E6%96%87", // 中文
            "/api/sessions/x%20y",              // 空格
            "/api/sessions/a%2Fb",              // 斜杠
        ] {
            let res = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::BAD_REQUEST, "uri={uri}");
        }
    }

    /// 不存在的会话 → 404（而非隐式创建）。
    #[tokio::test]
    async fn missing_session_404() {
        let app = test_app();
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/sessions/no-such-session-id")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    /// 废弃的模式接口对存在与不存在的会话都稳定返回 405，不探测会话状态。
    #[tokio::test]
    async fn deprecated_mode_update_is_stable_405() {
        let app = test_app();
        let res = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/sessions/no-such-session-id/mode")
                    .body(Body::from(r#"{"workflow_mode":"full"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    /// meta 只暴露前端展示所需的模型名与价格，不泄露完整配置。
    #[tokio::test]
    async fn meta_exposes_public_fields_only() {
        let app = test_app();
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/meta")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let dto = json_body(res).await;
        assert_eq!(dto["model"], "test");
        assert_eq!(dto["input_per_1m"], 0.0);
        assert_eq!(dto["output_per_1m"], 0.0);
        assert!(dto.get("api_key").is_none());
        assert!(dto.get("base_url").is_none());
    }

    /// sid 白名单规则本身。
    #[test]
    fn valid_sid_rules() {
        assert!(valid_sid("s19c9f"));
        assert!(valid_sid("a-b_C9"));
        assert!(!valid_sid(""));
        assert!(!valid_sid("../etc"));
        assert!(!valid_sid("有中文"));
        assert!(!valid_sid("a/b"));
        assert!(!valid_sid(&"x".repeat(65)));
        assert!(valid_sid(&"x".repeat(64)));
    }

    fn test_user(id: i64) -> UserRecord {
        UserRecord {
            id,
            username: format!("user-{id}"),
            password_hash: String::new(),
            role: Role::User,
            enabled: true,
            must_change_password: false,
            session_version: 0,
            last_login_at: None,
            failed_login_count: 0,
            locked_until: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    /// 认证开启后，列表只返回当前账号的会话，详情/修改接口拒绝别人的会话。
    #[tokio::test]
    async fn session_ownership_isolation() {
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
            auth: crate::config::AuthConfig {
                enabled: true,
                ..Default::default()
            },
        };
        let st: SharedState = Arc::new(AppState::new(
            cfg,
            llm::Client::for_test(),
            Arc::new(|_sid: &str| -> Vec<Box<crate::tools::DynTool>> { Vec::new() }),
        ));
        let sid_a = "test-owner-a";
        let sid_b = "test-owner-b";
        let sid_old = "test-owner-old";
        for (sid, owner) in [(sid_a, Some(11)), (sid_b, Some(22)), (sid_old, None)] {
            let mut session = Session::new(sid.into());
            session.owner_user_id = owner;
            session.save().unwrap();
        }

        let user_a = Extension(test_user(11));
        let result = list_sessions(State(st.clone()), Some(user_a.clone())).await;
        assert!(result.is_ok());
        let Json(list) = result.ok().unwrap_or_else(|| unreachable!());
        assert!(list.iter().any(|meta| meta.id == sid_a));
        assert!(!list.iter().any(|meta| meta.id == sid_b));
        assert!(!list.iter().any(|meta| meta.id == sid_old));

        assert!(load_owned_session(&st, sid_a, Some(&user_a)).is_ok());
        let user_b = Extension(test_user(22));
        let result = load_owned_session(&st, sid_a, Some(&user_b));
        assert_eq!(
            result.as_ref().err().map(|e| e.status),
            Some(StatusCode::NOT_FOUND)
        );

        for sid in [sid_a, sid_b, sid_old] {
            let _ = Session::delete(sid);
        }
    }
}
