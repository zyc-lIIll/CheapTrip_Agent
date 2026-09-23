//! M2-6 外部集成开关与小红书登录任务。
//!
//! 小红书登录任务只在管理员接口中暂存二维码，并在后台轮询登录状态；
//! 携程扫码与远程浏览器任务仍在后续工作包实现。

use axum::Extension;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::toggles::Toggles;
use crate::tools::HotelReviews;
use crate::tools::xhs::{XhsClient, XhsLoginQr};
use crate::web::auth::repository::{self, AuditEntry, UserRecord};
use crate::web::rest::{ApiError, bad_request};
use crate::web::state::SharedState;

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Serialize)]
pub(crate) struct IntegrationStatus {
    pub kind: &'static str,
    pub name: &'static str,
    pub configured_enabled: bool,
    pub desired_enabled: bool,
    pub effective_enabled: bool,
    pub login_state: &'static str,
    pub ready: bool,
    pub note: &'static str,
}

#[derive(Debug, Deserialize)]
pub(crate) struct EnabledRequest {
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct LoginTaskView {
    pub task_id: String,
    pub status: String,
    pub message: Option<String>,
    pub expires_at: u64,
    pub qr_available: bool,
}

struct LoginTask {
    kind: &'static str,
    status: String,
    message: Option<String>,
    expires_at: u64,
    qr: Option<XhsLoginQr>,
}

static LOGIN_TASKS: OnceLock<Mutex<HashMap<String, LoginTask>>> = OnceLock::new();

fn login_tasks() -> &'static Mutex<HashMap<String, LoginTask>> {
    LOGIN_TASKS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) async fn list(
    State(state): State<SharedState>,
    Extension(_actor): Extension<UserRecord>,
) -> Result<Json<Vec<IntegrationStatus>>, ApiError> {
    Ok(Json(statuses(&state)))
}

pub(crate) async fn set_enabled(
    State(state): State<SharedState>,
    Extension(actor): Extension<UserRecord>,
    Path(kind): Path<String>,
    Json(input): Json<EnabledRequest>,
) -> Result<Json<IntegrationStatus>, ApiError> {
    if !matches!(kind.as_str(), "xhs" | "hotel") {
        return Err(bad_request("未知集成类型，只支持 xhs 或 hotel"));
    }
    let key = if kind == "xhs" { "xhs" } else { "hotel" };
    let toggles = Toggles::set(key, input.enabled).map_err(ApiError::from)?;
    let backend = state.auth.as_deref().ok_or_else(|| ApiError {
        status: StatusCode::NOT_FOUND,
        msg: "账号登录尚未启用".into(),
    })?;
    let detail = format!("{{\"kind\":\"{kind}\",\"enabled\":{}}}", input.enabled);
    repository::record_audit(
        &backend.pool,
        AuditEntry {
            actor_user_id: Some(actor.id),
            actor_username: Some(&actor.username),
            action: "admin.integration.update_enabled",
            target_type: Some("integration"),
            target_id: Some(&kind),
            target_label: Some(&kind),
            result: "success",
            detail_json: Some(&detail),
        },
    )
    .await
    .map_err(ApiError::from)?;
    let status = statuses(&state)
        .into_iter()
        .find(|status| status.kind == kind)
        .ok_or_else(|| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            msg: "读取集成状态失败".into(),
        })?;
    let _ = toggles;
    Ok(Json(status))
}

/// POST /api/admin/integrations/xhs/login/start —— 获取二维码并启动后台状态轮询。
pub(crate) async fn start_xhs_login(
    State(state): State<SharedState>,
    Extension(actor): Extension<UserRecord>,
) -> Result<Json<LoginTaskView>, ApiError> {
    let task_id = tower_sessions::session::Id::default().to_string();
    let expires_at = unix_now().saturating_add(300);
    {
        let mut tasks = login_tasks().lock().map_err(|_| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            msg: "读取小红书登录任务失败".into(),
        })?;
        if tasks.values().any(|task| {
            task.kind == "xhs" && matches!(task.status.as_str(), "starting" | "waiting_for_scan")
        }) {
            return Err(ApiError {
                status: StatusCode::CONFLICT,
                msg: "已有一个小红书登录任务正在进行".into(),
            });
        }
        // 先登记 starting，再请求二维码，避免两个管理员并发点击时各自启动任务。
        tasks.insert(
            task_id.clone(),
            LoginTask {
                kind: "xhs",
                status: "starting".into(),
                message: Some("正在向小红书服务请求二维码…".into()),
                expires_at,
                qr: None,
            },
        );
    }
    let client = xhs_client(&state);
    let qr = match client.get_login_qrcode().await {
        Ok(qr) => qr,
        Err(error) => {
            update_task(
                &task_id,
                "failed",
                Some(format!("获取二维码失败：{error:#}")),
                None,
            );
            return Err(ApiError {
                status: StatusCode::BAD_GATEWAY,
                msg: format!("获取小红书登录二维码失败：{error:#}"),
            });
        }
    };
    let message = qr.text.clone();
    update_task(&task_id, "waiting_for_scan", message.clone(), Some(qr));
    let view = LoginTaskView {
        task_id: task_id.clone(),
        status: "waiting_for_scan".into(),
        message,
        expires_at,
        qr_available: true,
    };
    if let Err(error) =
        audit_integration(&state, &actor, "admin.integration.login_start", "xhs").await
    {
        update_task(
            &task_id,
            "failed",
            Some("登录任务审计失败，未启动轮询".into()),
            None,
        );
        return Err(error);
    }
    spawn_xhs_login_poller(task_id.clone(), client);
    Ok(Json(view))
}

/// GET /api/admin/integrations/xhs/login/{task_id}
pub(crate) async fn xhs_login_status(
    Path(task_id): Path<String>,
) -> Result<Json<LoginTaskView>, ApiError> {
    let tasks = login_tasks().lock().map_err(|_| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        msg: "读取小红书登录任务失败".into(),
    })?;
    let task = tasks.get(&task_id).ok_or_else(|| ApiError {
        status: StatusCode::NOT_FOUND,
        msg: "登录任务不存在或已过期".into(),
    })?;
    Ok(Json(task_view(&task_id, task)))
}

/// GET /api/admin/integrations/xhs/login/{task_id}/qr
pub(crate) async fn xhs_login_qr(Path(task_id): Path<String>) -> Result<Response, ApiError> {
    let tasks = login_tasks().lock().map_err(|_| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        msg: "读取小红书二维码失败".into(),
    })?;
    let task = tasks.get(&task_id).ok_or_else(|| ApiError {
        status: StatusCode::NOT_FOUND,
        msg: "登录任务不存在或已过期".into(),
    })?;
    let Some(qr) = &task.qr else {
        return Err(ApiError {
            status: StatusCode::GONE,
            msg: "二维码已失效".into(),
        });
    };
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &qr.data)
        .map_err(|_| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            msg: "二维码数据损坏".into(),
        })?;
    let content_type = header::HeaderValue::from_str(&qr.mime_type).map_err(|_| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        msg: "二维码类型非法".into(),
    })?;
    Ok(([(header::CONTENT_TYPE, content_type)], bytes).into_response())
}

/// POST /api/admin/integrations/xhs/logout
pub(crate) async fn logout_xhs(
    State(state): State<SharedState>,
    Extension(actor): Extension<UserRecord>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let client = xhs_client(&state);
    let message = client.delete_cookies().await.map_err(|error| ApiError {
        status: StatusCode::BAD_GATEWAY,
        msg: format!("小红书登出失败：{error:#}"),
    })?;
    clear_xhs_login_tasks();
    audit_integration(&state, &actor, "admin.integration.logout", "xhs").await?;
    Ok(Json(serde_json::json!({"ok": true, "message": message})))
}

fn xhs_client(state: &SharedState) -> XhsClient {
    let token = state
        .cfg
        .xhs
        .token_env
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .and_then(|name| std::env::var(name).ok())
        .filter(|value| !value.trim().is_empty());
    XhsClient::new(
        state.cfg.xhs.url.clone(),
        token,
        state.cfg.xhs.min_interval_ms,
        state.cfg.xhs.jitter_ms,
    )
}

fn xhs_login_state() -> &'static str {
    let Ok(tasks) = login_tasks().lock() else {
        return "unknown";
    };
    if tasks
        .values()
        .any(|task| task.kind == "xhs" && task.status == "success")
    {
        "logged_in"
    } else if tasks
        .values()
        .any(|task| task.kind == "xhs" && task.status == "waiting_for_scan")
    {
        "waiting_for_scan"
    } else if tasks
        .values()
        .any(|task| task.kind == "xhs" && task.status == "cancelled")
    {
        "not_logged_in"
    } else {
        "unknown"
    }
}

fn clear_xhs_login_tasks() {
    if let Ok(mut tasks) = login_tasks().lock() {
        for task in tasks.values_mut().filter(|task| task.kind == "xhs") {
            task.status = "cancelled".into();
            task.message = Some("已登出，请重新扫码".into());
            task.qr = None;
        }
    }
}

fn spawn_xhs_login_poller(task_id: String, client: XhsClient) {
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
        loop {
            if tokio::time::Instant::now() >= deadline {
                update_task(
                    &task_id,
                    "expired",
                    Some("二维码已过期，请重新获取".into()),
                    None,
                );
                break;
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
            match client.check_login_status().await {
                Ok(message) if message.contains("已登录") && !message.contains("未登录") => {
                    update_task(&task_id, "success", Some(message), None);
                    break;
                }
                Ok(message) => update_task(&task_id, "waiting_for_scan", Some(message), None),
                Err(error) => update_task(
                    &task_id,
                    "waiting_for_scan",
                    Some(format!("等待扫码（状态查询失败：{error:#}）")),
                    None,
                ),
            }
        }
        tokio::time::sleep(Duration::from_secs(600)).await;
        if let Ok(mut tasks) = login_tasks().lock() {
            tasks.remove(&task_id);
        }
    });
}

fn update_task(task_id: &str, status: &str, message: Option<String>, qr: Option<XhsLoginQr>) {
    if let Ok(mut tasks) = login_tasks().lock()
        && let Some(task) = tasks.get_mut(task_id)
    {
        task.status = status.into();
        task.message = message;
        if matches!(status, "success" | "failed" | "expired" | "cancelled") {
            task.qr = None;
        }
        if let Some(qr) = qr {
            task.qr = Some(qr);
        }
    }
}

fn task_view(task_id: &str, task: &LoginTask) -> LoginTaskView {
    LoginTaskView {
        task_id: task_id.into(),
        status: task.status.clone(),
        message: task.message.clone(),
        expires_at: task.expires_at,
        qr_available: task.qr.is_some(),
    }
}

async fn audit_integration(
    state: &SharedState,
    actor: &UserRecord,
    action: &str,
    kind: &str,
) -> Result<(), ApiError> {
    let backend = state.auth.as_deref().ok_or_else(|| ApiError {
        status: StatusCode::NOT_FOUND,
        msg: "账号登录尚未启用".into(),
    })?;
    repository::record_audit(
        &backend.pool,
        AuditEntry {
            actor_user_id: Some(actor.id),
            actor_username: Some(&actor.username),
            action,
            target_type: Some("integration"),
            target_id: Some(kind),
            target_label: Some(kind),
            result: "success",
            detail_json: None,
        },
    )
    .await
    .map_err(ApiError::from)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn statuses(state: &SharedState) -> Vec<IntegrationStatus> {
    let toggles = Toggles::load();
    let xhs_effective = toggles.xhs && state.cfg.xhs.enabled;
    let xhs_login = xhs_login_state();
    let hotel = HotelReviews::new("integration-status".into());
    let hotel_login = std::path::Path::new("scripts/hotel_crawl/.ctrip-state.json").exists();
    let hotel_ready = toggles.hotel_crawler && hotel.env_ready().is_ok();
    vec![
        IntegrationStatus {
            kind: "xhs",
            name: "小红书",
            configured_enabled: state.cfg.xhs.enabled,
            desired_enabled: toggles.xhs,
            effective_enabled: xhs_effective,
            login_state: xhs_login,
            ready: xhs_effective && xhs_login == "logged_in",
            note: "支持管理员扫码登录/登出；启用后重启 cheaptrip 生效",
        },
        IntegrationStatus {
            kind: "hotel",
            name: "携程酒店评论",
            configured_enabled: true,
            desired_enabled: toggles.hotel_crawler,
            effective_enabled: hotel_ready,
            login_state: if hotel_login {
                "logged_in"
            } else {
                "not_logged_in"
            },
            ready: hotel_ready,
            note: "需要本机 Playwright、脚本和携程登录态；启用后重启 cheaptrip 生效",
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integration_status_reflects_local_hotel_login_state() {
        let statuses = statuses_for_test();
        assert!(statuses.iter().any(|status| status.kind == "xhs"));
        assert!(statuses.iter().any(|status| status.kind == "hotel"));
    }

    fn statuses_for_test() -> Vec<IntegrationStatus> {
        let cfg = crate::config::Config {
            llm: crate::config::LlmConfig {
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
            cost: crate::config::CostConfig {
                input_per_1m: 0.0,
                output_per_1m: 0.0,
            },
            session: crate::config::SessionConfig {
                max_messages: None,
                max_sessions: None,
            },
            xhs: Default::default(),
            web: Default::default(),
            auth: Default::default(),
        };
        let state = SharedStateForTest::new(cfg);
        statuses(&state.0)
    }

    struct SharedStateForTest(std::sync::Arc<crate::web::state::AppState>);

    impl SharedStateForTest {
        fn new(cfg: crate::config::Config) -> Self {
            Self(std::sync::Arc::new(crate::web::state::AppState::new(
                cfg,
                crate::llm::Client::for_test(),
                std::sync::Arc::new(|_| Vec::new()),
            )))
        }
    }
}
