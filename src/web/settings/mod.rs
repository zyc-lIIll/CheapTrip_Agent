//! M2-5a 管理员设置接口。
//!
//! 这里只允许读写白名单内的非敏感覆盖项。真实密钥仍由进程环境读取，
//! 浏览器只能看到 configured 布尔值；保存后的覆盖项在服务重启时生效。

use std::env;

use anyhow::{Result, bail};
use axum::Extension;
use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::llm::provider::{Provider, ReasoningEffort};
use crate::toggles::Toggles;
use crate::web::auth::repository::{self, AuditEntry, UserRecord};
use crate::web::rest::{ApiError, bad_request};
use crate::web::state::SharedState;

const SETTING_KEYS: &[&str] = &[
    "llm.base_url",
    "llm.model",
    "llm.temperature",
    "llm.max_tokens",
    "llm.connect_timeout_secs",
    "llm.read_timeout_secs",
    "llm.provider",
    "llm.reasoning_effort",
    "cost.input_per_1m",
    "cost.output_per_1m",
    "web.max_concurrent",
];

const MODEL_AUDIT_DETAIL: &str = r#"{"fields":["model","llm_base_url","temperature","max_tokens","connect_timeout_secs","read_timeout_secs","provider","reasoning_effort","input_per_1m","output_per_1m","max_concurrent"]}"#;

/// 可返回浏览器的密钥状态。任何接口都不得返回真实密钥值。
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SecretStatus {
    pub name: &'static str,
    pub configured: bool,
}

/// 设置页允许展示/编辑的白名单；禁止直接序列化整个 Config 或 `.env`。
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AdminSettingsView {
    pub model: String,
    pub llm_base_url: String,
    pub temperature: f32,
    pub max_tokens: u32,
    pub connect_timeout_secs: u64,
    pub read_timeout_secs: u64,
    pub provider: String,
    pub reasoning_effort: String,
    pub input_per_1m: f64,
    pub output_per_1m: f64,
    pub max_concurrent: usize,
    pub xhs_enabled: bool,
    pub hotel_crawler_enabled: bool,
    pub secrets: Vec<SecretStatus>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ModelSettingsRequest {
    pub model: String,
    pub llm_base_url: String,
    pub temperature: f32,
    pub max_tokens: u32,
    pub connect_timeout_secs: u64,
    pub read_timeout_secs: u64,
    pub provider: String,
    pub reasoning_effort: String,
    pub input_per_1m: f64,
    pub output_per_1m: f64,
    pub max_concurrent: usize,
}

#[derive(Debug, Serialize)]
pub(crate) struct ModelSettingsResponse {
    #[serde(flatten)]
    pub settings: AdminSettingsView,
    pub restart_required: bool,
}

pub(crate) async fn get_settings(
    State(state): State<SharedState>,
    Extension(_actor): Extension<UserRecord>,
) -> Result<Json<AdminSettingsView>, ApiError> {
    Ok(Json(settings_view_with_overrides(&state).await?))
}

pub(crate) async fn get_secrets(
    State(state): State<SharedState>,
    Extension(_actor): Extension<UserRecord>,
) -> Result<Json<Vec<SecretStatus>>, ApiError> {
    Ok(Json(settings_view_with_overrides(&state).await?.secrets))
}

pub(crate) async fn update_model(
    State(state): State<SharedState>,
    Extension(actor): Extension<UserRecord>,
    Json(input): Json<ModelSettingsRequest>,
) -> Result<Json<ModelSettingsResponse>, ApiError> {
    validate(&input).map_err(bad_request)?;
    let backend = state.auth.as_deref().ok_or_else(|| ApiError {
        status: axum::http::StatusCode::NOT_FOUND,
        msg: "账号登录尚未启用".into(),
    })?;
    let values = vec![
        ("llm.base_url".into(), input.llm_base_url.trim().into()),
        ("llm.model".into(), input.model.trim().into()),
        ("llm.temperature".into(), input.temperature.to_string()),
        ("llm.max_tokens".into(), input.max_tokens.to_string()),
        (
            "llm.connect_timeout_secs".into(),
            input.connect_timeout_secs.to_string(),
        ),
        (
            "llm.read_timeout_secs".into(),
            input.read_timeout_secs.to_string(),
        ),
        ("llm.provider".into(), input.provider.clone()),
        (
            "llm.reasoning_effort".into(),
            input.reasoning_effort.clone(),
        ),
        ("cost.input_per_1m".into(), input.input_per_1m.to_string()),
        ("cost.output_per_1m".into(), input.output_per_1m.to_string()),
        (
            "web.max_concurrent".into(),
            input.max_concurrent.to_string(),
        ),
    ];
    repository::save_settings(&backend.pool, &values)
        .await
        .map_err(ApiError::from)?;
    repository::record_audit(
        &backend.pool,
        AuditEntry {
            actor_user_id: Some(actor.id),
            actor_username: Some(&actor.username),
            action: "admin.settings.update_model",
            target_type: Some("settings"),
            target_id: None,
            target_label: None,
            result: "success",
            detail_json: Some(MODEL_AUDIT_DETAIL),
        },
    )
    .await
    .map_err(ApiError::from)?;
    Ok(Json(ModelSettingsResponse {
        settings: settings_view_with_model(&state, &input),
        restart_required: true,
    }))
}

/// 在 Web 服务启动后加载 SQLite 覆盖项；配置文件仍是无覆盖时的默认值。
pub(crate) async fn apply_overrides(cfg: &mut Config, pool: &sqlx::SqlitePool) -> Result<()> {
    let values = repository::load_settings(pool).await?;
    for key in values.keys() {
        if !SETTING_KEYS.contains(&key.as_str()) {
            bail!("管理员设置包含未知键：{key}");
        }
    }
    cfg.llm.base_url = parse_or_default(&values, "llm.base_url", cfg.llm.base_url.clone())?;
    cfg.llm.model = parse_or_default(&values, "llm.model", cfg.llm.model.clone())?;
    cfg.llm.temperature = parse_or_default(&values, "llm.temperature", cfg.llm.temperature)?;
    cfg.llm.max_tokens = parse_or_default(&values, "llm.max_tokens", cfg.llm.max_tokens)?;
    cfg.llm.connect_timeout_secs = parse_or_default(
        &values,
        "llm.connect_timeout_secs",
        cfg.llm.connect_timeout_secs,
    )?;
    cfg.llm.read_timeout_secs =
        parse_or_default(&values, "llm.read_timeout_secs", cfg.llm.read_timeout_secs)?;
    cfg.llm.provider = parse_or_default(&values, "llm.provider", cfg.llm.provider)?;
    cfg.llm.reasoning_effort =
        parse_or_default(&values, "llm.reasoning_effort", cfg.llm.reasoning_effort)?;
    cfg.cost.input_per_1m = parse_or_default(&values, "cost.input_per_1m", cfg.cost.input_per_1m)?;
    cfg.cost.output_per_1m =
        parse_or_default(&values, "cost.output_per_1m", cfg.cost.output_per_1m)?;
    cfg.web.max_concurrent =
        parse_or_default(&values, "web.max_concurrent", cfg.web.max_concurrent)?;
    validate_config(cfg)
}

fn parse_or_default<T>(
    values: &std::collections::HashMap<String, String>,
    key: &str,
    default: T,
) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    values
        .get(key)
        .map(|value| {
            value
                .parse::<T>()
                .map_err(|error| anyhow::anyhow!("管理员设置 {key} 值非法：{error}"))
        })
        .unwrap_or(Ok(default))
}

fn validate(input: &ModelSettingsRequest) -> Result<(), String> {
    let base_url = input.llm_base_url.trim();
    if base_url.is_empty() || !(base_url.starts_with("http://") || base_url.starts_with("https://"))
    {
        return Err("LLM Base URL 必须是 http:// 或 https:// 地址".into());
    }
    if base_url.len() > 2048 {
        return Err("LLM Base URL 过长".into());
    }
    if input.model.trim().is_empty() || input.model.chars().count() > 128 {
        return Err("模型名不能为空且最多 128 个字符".into());
    }
    if !input.temperature.is_finite() || !(0.0..=2.0).contains(&input.temperature) {
        return Err("temperature 必须在 0 到 2 之间".into());
    }
    if input.max_tokens == 0 || input.max_tokens > 1_000_000 {
        return Err("max_tokens 必须在 1 到 1000000 之间".into());
    }
    if input.connect_timeout_secs > 600 || input.read_timeout_secs > 3600 {
        return Err("超时范围不合法".into());
    }
    if !input.input_per_1m.is_finite()
        || !input.output_per_1m.is_finite()
        || input.input_per_1m < 0.0
        || input.output_per_1m < 0.0
    {
        return Err("费用必须是非负数字".into());
    }
    if input.max_concurrent == 0 || input.max_concurrent > 64 {
        return Err("最大并发必须在 1 到 64 之间".into());
    }
    if input.provider.parse::<Provider>().is_err() {
        return Err("provider 必须是 auto、glm 或 openai_compatible".into());
    }
    if input.reasoning_effort.parse::<ReasoningEffort>().is_err() {
        return Err("reasoning_effort 必须是 low、high 或 max".into());
    }
    Ok(())
}

fn validate_config(cfg: &Config) -> Result<()> {
    let input = ModelSettingsRequest {
        model: cfg.llm.model.clone(),
        llm_base_url: cfg.llm.base_url.clone(),
        temperature: cfg.llm.temperature,
        max_tokens: cfg.llm.max_tokens,
        connect_timeout_secs: cfg.llm.connect_timeout_secs,
        read_timeout_secs: cfg.llm.read_timeout_secs,
        provider: cfg.llm.provider.to_string(),
        reasoning_effort: cfg.llm.reasoning_effort.to_string(),
        input_per_1m: cfg.cost.input_per_1m,
        output_per_1m: cfg.cost.output_per_1m,
        max_concurrent: cfg.web.max_concurrent,
    };
    validate(&input).map_err(|error| anyhow::anyhow!("管理员设置校验失败：{error}"))
}

fn settings_view(state: &SharedState) -> AdminSettingsView {
    let cfg = &state.cfg;
    AdminSettingsView {
        model: cfg.llm.model.clone(),
        llm_base_url: cfg.llm.base_url.clone(),
        temperature: cfg.llm.temperature,
        max_tokens: cfg.llm.max_tokens,
        connect_timeout_secs: cfg.llm.connect_timeout_secs,
        read_timeout_secs: cfg.llm.read_timeout_secs,
        provider: cfg.llm.provider.to_string(),
        reasoning_effort: cfg.llm.reasoning_effort.to_string(),
        input_per_1m: cfg.cost.input_per_1m,
        output_per_1m: cfg.cost.output_per_1m,
        max_concurrent: cfg.web.max_concurrent,
        xhs_enabled: cfg.xhs.enabled,
        hotel_crawler_enabled: Toggles::load().hotel_crawler,
        secrets: secret_statuses(cfg),
    }
}

fn settings_view_with_model(
    state: &SharedState,
    input: &ModelSettingsRequest,
) -> AdminSettingsView {
    let mut view = settings_view(state);
    view.model = input.model.trim().to_owned();
    view.llm_base_url = input.llm_base_url.trim().to_owned();
    view.temperature = input.temperature;
    view.max_tokens = input.max_tokens;
    view.connect_timeout_secs = input.connect_timeout_secs;
    view.read_timeout_secs = input.read_timeout_secs;
    view.provider = input.provider.clone();
    view.reasoning_effort = input.reasoning_effort.clone();
    view.input_per_1m = input.input_per_1m;
    view.output_per_1m = input.output_per_1m;
    view.max_concurrent = input.max_concurrent;
    view
}

async fn settings_view_with_overrides(state: &SharedState) -> Result<AdminSettingsView, ApiError> {
    let mut view = settings_view(state);
    let Some(backend) = state.auth.as_deref() else {
        return Ok(view);
    };
    let values = repository::load_settings(&backend.pool)
        .await
        .map_err(ApiError::from)?;
    view.llm_base_url =
        parse_or_default(&values, "llm.base_url", view.llm_base_url).map_err(ApiError::from)?;
    view.model = parse_or_default(&values, "llm.model", view.model).map_err(ApiError::from)?;
    view.temperature =
        parse_or_default(&values, "llm.temperature", view.temperature).map_err(ApiError::from)?;
    view.max_tokens =
        parse_or_default(&values, "llm.max_tokens", view.max_tokens).map_err(ApiError::from)?;
    view.connect_timeout_secs = parse_or_default(
        &values,
        "llm.connect_timeout_secs",
        view.connect_timeout_secs,
    )
    .map_err(ApiError::from)?;
    view.read_timeout_secs =
        parse_or_default(&values, "llm.read_timeout_secs", view.read_timeout_secs)
            .map_err(ApiError::from)?;
    view.provider =
        parse_or_default(&values, "llm.provider", view.provider).map_err(ApiError::from)?;
    view.reasoning_effort =
        parse_or_default(&values, "llm.reasoning_effort", view.reasoning_effort)
            .map_err(ApiError::from)?;
    view.input_per_1m = parse_or_default(&values, "cost.input_per_1m", view.input_per_1m)
        .map_err(ApiError::from)?;
    view.output_per_1m = parse_or_default(&values, "cost.output_per_1m", view.output_per_1m)
        .map_err(ApiError::from)?;
    view.max_concurrent = parse_or_default(&values, "web.max_concurrent", view.max_concurrent)
        .map_err(ApiError::from)?;
    Ok(view)
}

fn secret_statuses(cfg: &Config) -> Vec<SecretStatus> {
    vec![
        SecretStatus {
            name: "LLM API Key",
            configured: env::var(&cfg.llm.api_key_env).is_ok_and(|value| !value.trim().is_empty()),
        },
        SecretStatus {
            name: "高德 API Key",
            configured: env::var("AMAP_API_KEY").is_ok_and(|value| !value.trim().is_empty()),
        },
        SecretStatus {
            name: "Exa API Key",
            configured: env::var("EXA_API_KEY").is_ok_and(|value| !value.trim().is_empty()),
        },
        SecretStatus {
            name: "小红书 MCP Token",
            configured: cfg
                .xhs
                .token_env
                .as_deref()
                .and_then(|name| env::var(name).ok())
                .is_some_and(|value| !value.trim().is_empty()),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_settings_validation_rejects_unsafe_values() {
        let input = ModelSettingsRequest {
            model: "test".into(),
            llm_base_url: "file:///etc/passwd".into(),
            temperature: 0.7,
            max_tokens: 100,
            connect_timeout_secs: 1,
            read_timeout_secs: 1,
            provider: "auto".into(),
            reasoning_effort: "max".into(),
            input_per_1m: 0.0,
            output_per_1m: 0.0,
            max_concurrent: 1,
        };
        assert!(validate(&input).is_err());
    }

    #[test]
    fn model_settings_validation_rejects_unknown_provider_values() {
        let input = ModelSettingsRequest {
            model: "test".into(),
            llm_base_url: "https://example.test".into(),
            temperature: 0.7,
            max_tokens: 100,
            connect_timeout_secs: 1,
            read_timeout_secs: 1,
            provider: "deepseek".into(),
            reasoning_effort: "max".into(),
            input_per_1m: 0.0,
            output_per_1m: 0.0,
            max_concurrent: 1,
        };
        assert!(validate(&input).is_err());
    }

    #[test]
    fn model_settings_validation_rejects_unknown_reasoning_effort() {
        let input = ModelSettingsRequest {
            model: "test".into(),
            llm_base_url: "https://example.test".into(),
            temperature: 0.7,
            max_tokens: 100,
            connect_timeout_secs: 1,
            read_timeout_secs: 1,
            provider: "glm".into(),
            reasoning_effort: "off".into(),
            input_per_1m: 0.0,
            output_per_1m: 0.0,
            max_concurrent: 1,
        };
        assert!(validate(&input).is_err());
    }

    #[test]
    fn provider_settings_are_whitelisted_and_parseable_for_overrides() {
        assert!(SETTING_KEYS.contains(&"llm.provider"));
        assert!(SETTING_KEYS.contains(&"llm.reasoning_effort"));
        let values = std::collections::HashMap::from([
            ("llm.provider".to_owned(), "glm".to_owned()),
            ("llm.reasoning_effort".to_owned(), "low".to_owned()),
        ]);
        assert_eq!(
            parse_or_default(&values, "llm.provider", Provider::Auto).unwrap(),
            Provider::Glm
        );
        assert_eq!(
            parse_or_default(&values, "llm.reasoning_effort", ReasoningEffort::Max).unwrap(),
            ReasoningEffort::Low
        );
    }

    #[test]
    fn model_settings_audit_detail_names_provider_fields_without_secrets() {
        assert!(MODEL_AUDIT_DETAIL.contains("provider"));
        assert!(MODEL_AUDIT_DETAIL.contains("reasoning_effort"));
        assert!(!MODEL_AUDIT_DETAIL.contains("api_key"));
        assert!(!MODEL_AUDIT_DETAIL.contains("secret"));
    }
}
