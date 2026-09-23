//! 管理员用户管理 API（M2-4b）。
//!
//! 所有处理函数都挂在 `account_auth` 之后：只有已登录管理员能到达这里，
//! 临时密码只在创建/重置成功响应中出现一次，永不写入审计或日志。

use axum::Extension;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use super::AuthBackend;
use super::model::Role;
use super::password;
use super::repository::{self, AuditEntry, UserRecord};
use crate::session::Session;
use crate::web::rest::{ApiError, bad_request, not_found};
use crate::web::state::SharedState;

#[derive(Debug, Serialize)]
pub(crate) struct UserDto {
    pub id: i64,
    pub username: String,
    pub role: Role,
    pub enabled: bool,
    pub must_change_password: bool,
    pub last_login_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl From<UserRecord> for UserDto {
    fn from(user: UserRecord) -> Self {
        Self {
            id: user.id,
            username: user.username,
            role: user.role,
            enabled: user.enabled,
            must_change_password: user.must_change_password,
            last_login_at: user.last_login_at,
            created_at: user.created_at,
            updated_at: user.updated_at,
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreateUserRequest {
    pub username: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct TemporaryPasswordResponse {
    pub user: UserDto,
    pub temporary_password: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct UpdateUserRequest {
    pub enabled: Option<bool>,
    pub role: Option<Role>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DeleteDataAction {
    Permanent,
    Transfer,
}

#[derive(Debug, Deserialize)]
pub(crate) struct DeleteUserRequest {
    pub confirm_username: String,
    pub data_action: DeleteDataAction,
    pub transfer_to_admin_id: Option<i64>,
}

#[derive(Debug, Serialize)]
pub(crate) struct DeleteUserResponse {
    pub user: UserDto,
    pub deleted_sessions: usize,
    pub transferred_sessions: usize,
}

/// GET /api/admin/users
pub(crate) async fn list_users(
    State(state): State<SharedState>,
    Extension(_actor): Extension<UserRecord>,
) -> Result<Json<Vec<UserDto>>, ApiError> {
    let backend = backend(&state)?;
    let users = repository::list_users(&backend.pool)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(users.into_iter().map(UserDto::from).collect()))
}

/// POST /api/admin/users —— 只创建普通用户，密码由服务端生成并只返回一次。
pub(crate) async fn create_user(
    State(state): State<SharedState>,
    Extension(actor): Extension<UserRecord>,
    Json(input): Json<CreateUserRequest>,
) -> Result<Json<TemporaryPasswordResponse>, ApiError> {
    let backend = backend(&state)?;
    let username = input.username.trim();
    if username.is_empty() {
        return Err(bad_request("用户名不能为空"));
    }
    let temporary_password = temporary_password();
    let hash = password::hash_password(&temporary_password).map_err(ApiError::from)?;
    let user = repository::create_user(&backend.pool, username, &hash, Role::User)
        .await
        .map_err(map_repository_error)?;
    let target_id = user.id.to_string();
    audit(
        backend,
        &actor,
        AuditEntry {
            actor_user_id: Some(actor.id),
            actor_username: Some(&actor.username),
            action: "admin.user.create",
            target_type: Some("user"),
            target_id: Some(&target_id),
            target_label: Some(&user.username),
            result: "success",
            detail_json: None,
        },
    )
    .await?;
    Ok(Json(TemporaryPasswordResponse {
        user: user.into(),
        temporary_password,
    }))
}

/// PATCH /api/admin/users/{id} —— 更新启用状态或角色权限。
pub(crate) async fn update_user(
    State(state): State<SharedState>,
    Extension(actor): Extension<UserRecord>,
    Path(id): Path<i64>,
    Json(input): Json<UpdateUserRequest>,
) -> Result<Json<UserDto>, ApiError> {
    let backend = backend(&state)?;
    if input.enabled.is_none() && input.role.is_none() {
        return Err(bad_request("至少指定 enabled 或 role"));
    }
    if actor.id == id
        && (input.enabled == Some(false) || input.role.is_some_and(|role| role != Role::Admin))
    {
        return Err(bad_request("不能禁用或降级当前登录管理员"));
    }
    let user = repository::update_user(&backend.pool, id, input.enabled, input.role)
        .await
        .map_err(map_repository_error)?;
    let target_id = id.to_string();
    let detail = format!(
        "{{\"enabled\":{},\"role\":{}}}",
        input.enabled.map_or("null".into(), |v| v.to_string()),
        input
            .role
            .map_or_else(|| "null".into(), |role| format!("\"{}\"", role.as_str()))
    );
    audit(
        backend,
        &actor,
        AuditEntry {
            actor_user_id: Some(actor.id),
            actor_username: Some(&actor.username),
            action: "admin.user.update",
            target_type: Some("user"),
            target_id: Some(&target_id),
            target_label: Some(&user.username),
            result: "success",
            detail_json: Some(&detail),
        },
    )
    .await?;
    Ok(Json(user.into()))
}

/// POST /api/admin/users/{id}/reset-password
pub(crate) async fn reset_password(
    State(state): State<SharedState>,
    Extension(actor): Extension<UserRecord>,
    Path(id): Path<i64>,
) -> Result<Json<TemporaryPasswordResponse>, ApiError> {
    let backend = backend(&state)?;
    let target = repository::find_by_id(&backend.pool, id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| not_found(format!("用户不存在：{id}")))?;
    let temporary_password = temporary_password();
    let hash = password::hash_password(&temporary_password).map_err(ApiError::from)?;
    let user = repository::reset_password(&backend.pool, &target.username, &hash)
        .await
        .map_err(map_repository_error)?;
    let target_id = id.to_string();
    audit(
        backend,
        &actor,
        AuditEntry {
            actor_user_id: Some(actor.id),
            actor_username: Some(&actor.username),
            action: "admin.user.reset_password",
            target_type: Some("user"),
            target_id: Some(&target_id),
            target_label: Some(&user.username),
            result: "success",
            detail_json: None,
        },
    )
    .await?;
    Ok(Json(TemporaryPasswordResponse {
        user: user.into(),
        temporary_password,
    }))
}

/// POST /api/admin/users/{id}/revoke-sessions
pub(crate) async fn revoke_sessions(
    State(state): State<SharedState>,
    Extension(actor): Extension<UserRecord>,
    Path(id): Path<i64>,
) -> Result<Json<UserDto>, ApiError> {
    let backend = backend(&state)?;
    let user = repository::revoke_sessions(&backend.pool, id)
        .await
        .map_err(map_repository_error)?;
    let target_id = id.to_string();
    audit(
        backend,
        &actor,
        AuditEntry {
            actor_user_id: Some(actor.id),
            actor_username: Some(&actor.username),
            action: "admin.user.revoke_sessions",
            target_type: Some("user"),
            target_id: Some(&target_id),
            target_label: Some(&user.username),
            result: "success",
            detail_json: None,
        },
    )
    .await?;
    Ok(Json(user.into()))
}

/// POST /api/admin/users/{id}/delete —— 删除普通用户及其旅行数据，或转移给管理员。
pub(crate) async fn delete_user(
    State(state): State<SharedState>,
    Extension(actor): Extension<UserRecord>,
    Path(id): Path<i64>,
    Json(input): Json<DeleteUserRequest>,
) -> Result<Json<DeleteUserResponse>, ApiError> {
    let backend = backend(&state)?;
    let target = repository::find_by_id(&backend.pool, id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| not_found(format!("用户不存在：{id}")))?;
    if target.role != Role::User {
        return Err(bad_request("只能删除普通用户"));
    }
    if input.confirm_username.trim() != target.username {
        return Err(bad_request("确认用户名不匹配"));
    }
    let transfer_admin = match input.data_action {
        DeleteDataAction::Permanent => None,
        DeleteDataAction::Transfer => {
            let admin_id = input
                .transfer_to_admin_id
                .ok_or_else(|| bad_request("请选择接收数据的管理员"))?;
            if admin_id == id {
                return Err(bad_request("不能把数据转移给待删除用户"));
            }
            let admin = repository::find_by_id(&backend.pool, admin_id)
                .await
                .map_err(ApiError::from)?
                .ok_or_else(|| bad_request("接收账号不存在"))?;
            if admin.role != Role::Admin || !admin.enabled {
                return Err(bad_request("接收账号必须是启用中的管理员"));
            }
            Some(admin.id)
        }
    };

    let disabled = repository::disable_for_deletion(&backend.pool, id)
        .await
        .map_err(map_repository_error)?;
    let session_ids = Session::list()
        .map_err(ApiError::from)?
        .into_iter()
        .filter(|meta| meta.owner_user_id == Some(id))
        .map(|meta| meta.id)
        .collect::<Vec<_>>();

    let mut quiesced: Vec<String> = Vec::with_capacity(session_ids.len());
    for sid in &session_ids {
        if let Err(error) = state.quiesce_session(sid).await {
            for previous in &quiesced {
                let _ = state.release_quiesced(previous);
            }
            return Err(ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                msg: format!("用户已禁用，暂停会话 {sid} 失败，数据未删除：{error}"),
            });
        }
        quiesced.push(sid.clone());
    }

    let mut failures = Vec::new();
    let mut deleted_sessions = 0;
    let mut transferred_sessions = 0;
    for sid in &quiesced {
        let result = if let Some(admin_id) = transfer_admin {
            Session::transfer_owner(sid, admin_id)
        } else {
            Session::delete(sid)
        };
        match result {
            Ok(()) => {
                if transfer_admin.is_some() {
                    transferred_sessions += 1;
                } else {
                    deleted_sessions += 1;
                }
            }
            Err(error) => failures.push(format!("{sid}: {error}")),
        }
    }
    for sid in &quiesced {
        let _ = state.release_quiesced(sid);
    }
    if !failures.is_empty() {
        return Err(ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            msg: format!(
                "用户已禁用，数据处理失败；请修复后重试。失败会话：{}",
                failures.join("；")
            ),
        });
    }

    repository::delete_user(&backend.pool, id)
        .await
        .map_err(map_repository_error)?;
    let target_id = id.to_string();
    let detail = format!(
        "{{\"data_action\":\"{}\",\"deleted_sessions\":{},\"transferred_sessions\":{}}}",
        if transfer_admin.is_some() {
            "transfer"
        } else {
            "permanent"
        },
        deleted_sessions,
        transferred_sessions
    );
    audit(
        backend,
        &actor,
        AuditEntry {
            actor_user_id: Some(actor.id),
            actor_username: Some(&actor.username),
            action: "admin.user.delete",
            target_type: Some("user"),
            target_id: Some(&target_id),
            target_label: Some(&target.username),
            result: "success",
            detail_json: Some(&detail),
        },
    )
    .await?;
    Ok(Json(DeleteUserResponse {
        user: disabled.into(),
        deleted_sessions,
        transferred_sessions,
    }))
}

fn backend(state: &SharedState) -> Result<&AuthBackend, ApiError> {
    state
        .auth
        .as_deref()
        .ok_or_else(|| not_found("账号登录尚未启用"))
}

async fn audit(
    backend: &AuthBackend,
    _actor: &UserRecord,
    entry: AuditEntry<'_>,
) -> Result<(), ApiError> {
    repository::record_audit(&backend.pool, entry)
        .await
        .map_err(ApiError::from)
}

fn temporary_password() -> String {
    format!("Tmp-{}", tower_sessions::session::Id::default())
}

fn map_repository_error(error: anyhow::Error) -> ApiError {
    let message = error.to_string();
    let status = if message.contains("用户名已存在") {
        StatusCode::CONFLICT
    } else if message.contains("用户不存在") {
        StatusCode::NOT_FOUND
    } else if message.contains("不能") || message.contains("至少指定") {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    ApiError {
        status,
        msg: message,
    }
}
