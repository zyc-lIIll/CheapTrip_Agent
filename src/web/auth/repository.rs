//! M2-1a 认证仓储：用户查询/创建与安全审计写入。
//!
//! 本模块只返回内部用户记录；密码哈希永远不会被序列化到 Web DTO。

use anyhow::{Context, Result, anyhow, bail};
use sqlx::{FromRow, SqlitePool};
use std::collections::HashMap;

use super::model::Role;

#[derive(Debug, Clone)]
pub(crate) struct UserRecord {
    pub id: i64,
    pub username: String,
    pub password_hash: String,
    pub role: Role,
    pub enabled: bool,
    pub must_change_password: bool,
    pub session_version: i64,
    pub last_login_at: Option<i64>,
    pub failed_login_count: i64,
    pub locked_until: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, FromRow)]
struct UserRow {
    id: i64,
    username: String,
    password_hash: String,
    role: String,
    enabled: i64,
    must_change_password: i64,
    session_version: i64,
    last_login_at: Option<i64>,
    failed_login_count: i64,
    locked_until: Option<i64>,
    created_at: i64,
    updated_at: i64,
}

impl TryFrom<UserRow> for UserRecord {
    type Error = anyhow::Error;

    fn try_from(row: UserRow) -> Result<Self, Self::Error> {
        let role = match row.role.as_str() {
            "admin" => Role::Admin,
            "user" => Role::User,
            other => bail!("用户 {} 的角色值非法：{other:?}", row.id),
        };
        Ok(Self {
            id: row.id,
            username: row.username,
            password_hash: row.password_hash,
            role,
            enabled: row.enabled != 0,
            must_change_password: row.must_change_password != 0,
            session_version: row.session_version,
            last_login_at: row.last_login_at,
            failed_login_count: row.failed_login_count,
            locked_until: row.locked_until,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

const USER_COLUMNS: &str = "id, username, password_hash, role, enabled, must_change_password, \
    session_version, last_login_at, failed_login_count, locked_until, created_at, updated_at";

async fn load_by_id(pool: &SqlitePool, id: i64) -> Result<Option<UserRecord>> {
    let sql = format!("SELECT {USER_COLUMNS} FROM users WHERE id = ?");
    let row = sqlx::query_as::<_, UserRow>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await
        .context("按 id 查询用户失败")?;
    row.map(UserRecord::try_from).transpose()
}

/// 按大小写不敏感的用户名查询用户。
pub(crate) async fn find_by_username(
    pool: &SqlitePool,
    username: &str,
) -> Result<Option<UserRecord>> {
    let sql = format!("SELECT {USER_COLUMNS} FROM users WHERE username = ? COLLATE NOCASE");
    let row = sqlx::query_as::<_, UserRow>(&sql)
        .bind(username)
        .fetch_optional(pool)
        .await
        .context("按用户名查询用户失败")?;
    row.map(UserRecord::try_from).transpose()
}

/// 按主键查询用户。
pub(crate) async fn find_by_id(pool: &SqlitePool, id: i64) -> Result<Option<UserRecord>> {
    load_by_id(pool, id).await
}

/// 列出用户；密码哈希只存在内部记录，调用方不得直接转成 Web 响应。
pub(crate) async fn list_users(pool: &SqlitePool) -> Result<Vec<UserRecord>> {
    let sql = format!("SELECT {USER_COLUMNS} FROM users ORDER BY id ASC");
    let rows = sqlx::query_as::<_, UserRow>(&sql)
        .fetch_all(pool)
        .await
        .context("列出用户失败")?;
    rows.into_iter().map(UserRecord::try_from).collect()
}

pub(crate) async fn count_admins(pool: &SqlitePool) -> Result<i64> {
    sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE role = 'admin'")
        .fetch_one(pool)
        .await
        .context("统计管理员失败")
}

pub(crate) async fn mark_login(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query(
        "UPDATE users SET last_login_at = unixepoch(), failed_login_count = 0,
         locked_until = NULL, updated_at = unixepoch() WHERE id = ?",
    )
    .bind(id)
    .execute(pool)
    .await
    .context("更新用户登录时间失败")?;
    Ok(())
}

pub(crate) async fn record_login_failure(
    pool: &SqlitePool,
    id: i64,
    max_failures: i64,
    lockout_secs: i64,
) -> Result<bool> {
    let max_failures = max_failures.max(1);
    let lockout_secs = lockout_secs.max(1);
    let result = sqlx::query(
        "UPDATE users SET failed_login_count = failed_login_count + 1,
         locked_until = CASE WHEN failed_login_count + 1 >= ?
             THEN unixepoch() + ?
             WHEN locked_until > unixepoch() THEN locked_until
             ELSE NULL END,
         updated_at = unixepoch() WHERE id = ?",
    )
    .bind(max_failures)
    .bind(lockout_secs)
    .bind(id)
    .execute(pool)
    .await
    .context("记录登录失败次数失败")?;
    if result.rows_affected() == 0 {
        bail!("记录登录失败时用户不存在：{id}");
    }
    let locked: Option<i64> = sqlx::query_scalar("SELECT locked_until FROM users WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .context("读取登录锁定状态失败")?;
    Ok(locked.is_some_and(|until| until > 0))
}

pub(crate) async fn change_password(
    pool: &SqlitePool,
    id: i64,
    password_hash: &str,
) -> Result<i64> {
    if password_hash.trim().is_empty() {
        bail!("密码哈希不能为空");
    }
    let result = sqlx::query(
        "UPDATE users SET password_hash = ?, must_change_password = 0,
         session_version = session_version + 1, failed_login_count = 0,
         locked_until = NULL, updated_at = unixepoch() WHERE id = ?",
    )
    .bind(password_hash)
    .bind(id)
    .execute(pool)
    .await
    .context("修改密码失败")?;
    if result.rows_affected() == 0 {
        bail!("用户不存在：{id}");
    }
    sqlx::query_scalar("SELECT session_version FROM users WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .context("读取新会话版本失败")
}

pub(crate) async fn reset_password(
    pool: &SqlitePool,
    username: &str,
    password_hash: &str,
) -> Result<UserRecord> {
    let username = username.trim();
    if username.is_empty() || password_hash.trim().is_empty() {
        bail!("用户名和密码哈希不能为空");
    }
    let result = sqlx::query(
        "UPDATE users SET password_hash = ?, must_change_password = 1,
         session_version = session_version + 1, failed_login_count = 0,
         locked_until = NULL, updated_at = unixepoch() WHERE username = ? COLLATE NOCASE",
    )
    .bind(password_hash)
    .bind(username)
    .execute(pool)
    .await
    .context("重置用户密码失败")?;
    if result.rows_affected() == 0 {
        bail!("用户不存在：{username}");
    }
    find_by_username(pool, username)
        .await?
        .ok_or_else(|| anyhow!("重置后找不到用户：{username}"))
}

/// 管理员更新用户状态/角色。任何实际变更都会递增 session_version，使旧登录失效。
pub(crate) async fn update_user(
    pool: &SqlitePool,
    id: i64,
    enabled: Option<bool>,
    role: Option<Role>,
) -> Result<UserRecord> {
    if enabled.is_none() && role.is_none() {
        return find_by_id(pool, id)
            .await?
            .ok_or_else(|| anyhow!("用户不存在：{id}"));
    }
    let mut tx = pool.begin().await.context("开始更新用户事务失败")?;
    let sql = format!("SELECT {USER_COLUMNS} FROM users WHERE id = ?");
    let target = sqlx::query_as::<_, UserRow>(&sql)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .context("读取待更新用户失败")?
        .map(UserRecord::try_from)
        .transpose()?
        .ok_or_else(|| anyhow!("用户不存在：{id}"))?;
    let next_role = role.unwrap_or(target.role);
    let next_enabled = enabled.unwrap_or(target.enabled);
    if target.role == Role::Admin && target.enabled && next_role != Role::Admin {
        let admins: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE role = 'admin' AND enabled = 1")
                .fetch_one(&mut *tx)
                .await
                .context("统计有效管理员失败")?;
        if admins <= 1 {
            bail!("不能降级最后一个有效管理员");
        }
    }
    if target.role == Role::Admin && target.enabled && !next_enabled {
        let admins: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE role = 'admin' AND enabled = 1")
                .fetch_one(&mut *tx)
                .await
                .context("统计有效管理员失败")?;
        if admins <= 1 {
            bail!("不能禁用最后一个有效管理员");
        }
    }
    if target.role == next_role && target.enabled == next_enabled {
        tx.rollback().await.ok();
        return Ok(target);
    }
    sqlx::query(
        "UPDATE users SET enabled = ?, role = ?, session_version = session_version + 1,
         failed_login_count = 0, locked_until = NULL, updated_at = unixepoch() WHERE id = ?",
    )
    .bind(i64::from(next_enabled))
    .bind(next_role.as_str())
    .bind(id)
    .execute(&mut *tx)
    .await
    .context("更新用户状态失败")?;
    let updated = sqlx::query_as::<_, UserRow>(&sql)
        .bind(id)
        .fetch_one(&mut *tx)
        .await
        .context("读取更新后的用户失败")?
        .try_into()?;
    tx.commit().await.context("提交用户更新事务失败")?;
    Ok(updated)
}

/// 撤销用户的全部登录 Session，而不修改账号状态。
pub(crate) async fn revoke_sessions(pool: &SqlitePool, id: i64) -> Result<UserRecord> {
    let result = sqlx::query(
        "UPDATE users SET session_version = session_version + 1,
         updated_at = unixepoch() WHERE id = ?",
    )
    .bind(id)
    .execute(pool)
    .await
    .context("撤销用户登录失败")?;
    if result.rows_affected() == 0 {
        bail!("用户不存在：{id}");
    }
    find_by_id(pool, id)
        .await?
        .ok_or_else(|| anyhow!("撤销后找不到用户：{id}"))
}

/// 删除前禁用普通用户并递增会话版本；后续文件处理失败时账号保持禁用，便于重试。
pub(crate) async fn disable_for_deletion(pool: &SqlitePool, id: i64) -> Result<UserRecord> {
    let mut tx = pool.begin().await.context("开始准备删除用户事务失败")?;
    let sql = format!("SELECT {USER_COLUMNS} FROM users WHERE id = ?");
    let target = sqlx::query_as::<_, UserRow>(&sql)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .context("读取待删除用户失败")?
        .map(UserRecord::try_from)
        .transpose()?
        .ok_or_else(|| anyhow!("用户不存在：{id}"))?;
    if target.role != Role::User {
        bail!("只能删除普通用户");
    }
    if target.enabled {
        sqlx::query(
            "UPDATE users SET enabled = 0, session_version = session_version + 1,
             failed_login_count = 0, locked_until = NULL, updated_at = unixepoch() WHERE id = ?",
        )
        .bind(id)
        .execute(&mut *tx)
        .await
        .context("禁用待删除用户失败")?;
    }
    let updated = sqlx::query_as::<_, UserRow>(&sql)
        .bind(id)
        .fetch_one(&mut *tx)
        .await
        .context("读取禁用后的用户失败")?
        .try_into()?;
    tx.commit().await.context("提交删除用户准备事务失败")?;
    Ok(updated)
}

/// 最终删除用户记录。仅接受已禁用的普通用户，避免并发管理操作误删管理员。
pub(crate) async fn delete_user(pool: &SqlitePool, id: i64) -> Result<()> {
    let result = sqlx::query("DELETE FROM users WHERE id = ? AND role = 'user' AND enabled = 0")
        .bind(id)
        .execute(pool)
        .await
        .context("删除用户记录失败")?;
    if result.rows_affected() == 0 {
        bail!("用户不存在、不是普通用户或尚未禁用：{id}");
    }
    Ok(())
}

/// 读取管理员设置覆盖项；键集合由 settings 模块在读写前校验。
pub(crate) async fn load_settings(pool: &SqlitePool) -> Result<HashMap<String, String>> {
    sqlx::query_as::<_, (String, String)>("SELECT key, value FROM app_settings")
        .fetch_all(pool)
        .await
        .context("读取管理员设置失败")
        .map(|rows| rows.into_iter().collect())
}

/// 原子写入管理员设置覆盖项。
pub(crate) async fn save_settings(pool: &SqlitePool, values: &[(String, String)]) -> Result<()> {
    let mut tx = pool.begin().await.context("开始保存管理员设置事务失败")?;
    for (key, value) in values {
        sqlx::query(
            "INSERT INTO app_settings (key, value, updated_at) VALUES (?, ?, unixepoch())
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = unixepoch()",
        )
        .bind(key)
        .bind(value)
        .execute(&mut *tx)
        .await
        .with_context(|| format!("保存管理员设置 {key} 失败"))?;
    }
    tx.commit().await.context("提交管理员设置事务失败")?;
    Ok(())
}

/// 创建用户。用户名由数据库唯一约束保证不重复，密码参数必须已经是 Argon2id 哈希。
pub(crate) async fn create_user(
    pool: &SqlitePool,
    username: &str,
    password_hash: &str,
    role: Role,
) -> Result<UserRecord> {
    let username = username.trim();
    if username.is_empty() {
        bail!("用户名不能为空");
    }
    if username.chars().count() > 64 {
        bail!("用户名过长（最多 64 个字符）");
    }
    if password_hash.trim().is_empty() {
        bail!("密码哈希不能为空");
    }

    let mut tx = pool.begin().await.context("开始创建用户事务失败")?;
    let result = sqlx::query("INSERT INTO users (username, password_hash, role) VALUES (?, ?, ?)")
        .bind(username)
        .bind(password_hash)
        .bind(role.as_str())
        .execute(&mut *tx)
        .await;
    let result = match result {
        Ok(result) => result,
        Err(error)
            if error
                .as_database_error()
                .is_some_and(|error| error.is_unique_violation()) =>
        {
            return Err(anyhow!("用户名已存在：{username}"));
        }
        Err(error) => return Err(error).context("创建用户失败"),
    };
    let id = result.last_insert_rowid();
    let sql = format!("SELECT {USER_COLUMNS} FROM users WHERE id = ?");
    let row = sqlx::query_as::<_, UserRow>(&sql)
        .bind(id)
        .fetch_one(&mut *tx)
        .await
        .context("读取刚创建的用户失败")?;
    tx.commit().await.context("提交创建用户事务失败")?;
    row.try_into()
}

/// 一条安全审计记录。detail_json 必须已经过调用方脱敏；None 表示没有附加详情。
pub(crate) struct AuditEntry<'a> {
    pub actor_user_id: Option<i64>,
    pub actor_username: Option<&'a str>,
    pub action: &'a str,
    pub target_type: Option<&'a str>,
    pub target_id: Option<&'a str>,
    pub target_label: Option<&'a str>,
    pub result: &'a str,
    pub detail_json: Option<&'a str>,
}

/// 写入安全审计。
pub(crate) async fn record_audit(pool: &SqlitePool, entry: AuditEntry<'_>) -> Result<()> {
    if entry.action.trim().is_empty() || entry.result.trim().is_empty() {
        bail!("审计 action/result 不能为空");
    }
    sqlx::query(
        "INSERT INTO audit_logs
         (actor_user_id, actor_username, action, target_type, target_id, target_label, result, detail_json)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(entry.actor_user_id)
    .bind(entry.actor_username)
    .bind(entry.action)
    .bind(entry.target_type)
    .bind(entry.target_id)
    .bind(entry.target_label)
    .bind(entry.result)
    .bind(entry.detail_json)
    .execute(pool)
    .await
    .context("写入安全审计失败")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::auth::AuthBackend;

    async fn backend() -> AuthBackend {
        AuthBackend::connect(&crate::config::AuthConfig {
            enabled: true,
            database_url: "sqlite::memory:".into(),
            ..Default::default()
        })
        .await
        .expect("测试数据库应能初始化")
    }

    #[tokio::test]
    async fn user_repository_roundtrip_and_audit() -> Result<()> {
        let backend = backend().await;
        let created = create_user(&backend.pool, " Alice ", "$argon2id$hash", Role::Admin).await?;
        assert_eq!(created.username, "Alice");
        assert_eq!(created.role, Role::Admin);
        assert!(created.enabled);
        assert!(created.must_change_password);
        assert_eq!(created.session_version, 1);

        let by_name = find_by_username(&backend.pool, "alice")
            .await?
            .expect("用户应存在");
        assert_eq!(by_name.id, created.id);
        assert_eq!(
            find_by_id(&backend.pool, created.id)
                .await?
                .unwrap()
                .username,
            "Alice"
        );
        assert_eq!(list_users(&backend.pool).await?.len(), 1);

        let target_id = created.id.to_string();
        record_audit(
            &backend.pool,
            AuditEntry {
                actor_user_id: Some(created.id),
                actor_username: Some("Alice"),
                action: "user.create",
                target_type: Some("user"),
                target_id: Some(&target_id),
                target_label: Some("Alice"),
                result: "success",
                detail_json: Some(r#"{"source":"test"}"#),
            },
        )
        .await?;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_logs")
            .fetch_one(&backend.pool)
            .await?;
        assert_eq!(count, 1);
        Ok(())
    }

    #[tokio::test]
    async fn duplicate_user_is_rejected() -> Result<()> {
        let backend = backend().await;
        create_user(&backend.pool, "alice", "$argon2id$hash", Role::User).await?;
        let error = create_user(&backend.pool, "ALICE", "$argon2id$hash", Role::User)
            .await
            .expect_err("大小写不同的重复用户名应被拒绝");
        assert!(error.to_string().contains("用户名已存在"));
        Ok(())
    }

    #[tokio::test]
    async fn admin_user_controls_revoke_and_last_admin_guards() -> Result<()> {
        let backend = backend().await;
        let admin_hash = "$argon2id$admin";
        let admin = create_user(&backend.pool, "admin", admin_hash, Role::Admin).await?;
        let user = create_user(&backend.pool, "alice", "$argon2id$user", Role::User).await?;

        let updated = update_user(&backend.pool, user.id, Some(false), Some(Role::Admin)).await?;
        assert!(!updated.enabled);
        assert_eq!(updated.role, Role::Admin);
        assert_eq!(updated.session_version, user.session_version + 1);

        let reenabled = update_user(&backend.pool, user.id, Some(true), Some(Role::User)).await?;
        assert!(reenabled.enabled);
        assert_eq!(reenabled.role, Role::User);

        let revoked = revoke_sessions(&backend.pool, user.id).await?;
        assert_eq!(revoked.session_version, reenabled.session_version + 1);

        let error = update_user(&backend.pool, admin.id, Some(false), None)
            .await
            .expect_err("最后一个有效管理员不能被禁用");
        assert!(error.to_string().contains("最后一个有效管理员"));
        Ok(())
    }

    #[tokio::test]
    async fn user_deletion_requires_disabled_regular_account() -> Result<()> {
        let backend = backend().await;
        let admin = create_user(&backend.pool, "admin", "$argon2id$admin", Role::Admin).await?;
        let user = create_user(&backend.pool, "alice", "$argon2id$user", Role::User).await?;

        let disabled = disable_for_deletion(&backend.pool, user.id).await?;
        assert!(!disabled.enabled);
        assert_eq!(disabled.session_version, user.session_version + 1);
        delete_user(&backend.pool, user.id).await?;
        assert!(find_by_id(&backend.pool, user.id).await?.is_none());

        let error = disable_for_deletion(&backend.pool, admin.id)
            .await
            .expect_err("管理员不能进入删除流程");
        assert!(error.to_string().contains("只能删除普通用户"));
        Ok(())
    }

    #[tokio::test]
    async fn app_settings_are_saved_atomically() -> Result<()> {
        let backend = backend().await;
        save_settings(
            &backend.pool,
            &[
                ("llm.model".into(), "model-v2".into()),
                ("web.max_concurrent".into(), "4".into()),
            ],
        )
        .await?;
        let values = load_settings(&backend.pool).await?;
        assert_eq!(
            values.get("llm.model").map(String::as_str),
            Some("model-v2")
        );
        assert_eq!(
            values.get("web.max_concurrent").map(String::as_str),
            Some("4")
        );
        Ok(())
    }
}
