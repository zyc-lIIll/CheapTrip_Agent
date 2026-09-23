use anyhow::{Context, Result};
use serde::Deserialize;
use std::env;

use crate::llm::provider::{Provider, ReasoningEffort};

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub llm: LlmConfig,
    pub cost: CostConfig,
    pub session: SessionConfig,
    /// [xhs] 小红书 MCP 服务（xiaohongshu-mcp 本机服务），缺省不启用
    #[serde(default)]
    pub xhs: XhsConfig,
    /// [web] HTTP/WS 服务（M0：axum 包 Core），缺省 127.0.0.1:8080
    #[serde(default)]
    pub web: WebConfig,
    /// [auth] M2 账号与服务端 Session；默认关闭，骨架完成前不挂公开路由。
    #[serde(default)]
    #[allow(dead_code)]
    pub auth: AuthConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct LlmConfig {
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
    pub temperature: f32,
    pub max_tokens: u32,
    /// 连接建立超时（秒），默认 10
    #[serde(default = "default_connect_timeout_secs")]
    pub connect_timeout_secs: u64,
    /// 空闲读取超时（秒）：连续无数据才计时，每个数据块到达即重置；0 = 不设。默认 60
    #[serde(default = "default_read_timeout_secs")]
    pub read_timeout_secs: u64,
    /// 请求供应商适配器；auto 会按模型名识别 GLM，否则使用通用协议。
    #[serde(default)]
    pub provider: Provider,
    /// GLM 思考强度；旧配置默认保持 max 行为。
    #[serde(default)]
    pub reasoning_effort: ReasoningEffort,
}

fn default_connect_timeout_secs() -> u64 {
    10
}

fn default_read_timeout_secs() -> u64 {
    60
}

#[derive(Debug, Deserialize, Clone)]
pub struct CostConfig {
    pub input_per_1m: f64,
    pub output_per_1m: f64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SessionConfig {
    /// None 表示不限（正无穷）
    #[serde(default)]
    pub max_messages: Option<usize>,
    #[serde(default)]
    pub max_sessions: Option<usize>,
}

/// [xhs] 小红书 MCP（xiaohongshu-mcp）本机服务接入。
/// enabled=false 时相关工具不注册，LLM 看不到。
#[derive(Debug, Deserialize, Clone)]
pub struct XhsConfig {
    #[serde(default)]
    pub enabled: bool,
    /// MCP StreamableHTTP 端点
    #[serde(default = "default_xhs_url")]
    pub url: String,
    /// 服务开启 Bearer 认证时，从哪个环境变量读 token；None/空 = 不带认证
    #[serde(default)]
    pub token_env: Option<String>,
    /// 拟人限流：相邻 MCP 调用最小间隔（毫秒），默认 8000
    #[serde(default = "default_xhs_min_interval_ms")]
    pub min_interval_ms: u64,
    /// 拟人限流：随机抖动上限（毫秒），实际间隔 = min + rand(0..jitter)，默认 7000
    #[serde(default = "default_xhs_jitter_ms")]
    pub jitter_ms: u64,
}

fn default_xhs_min_interval_ms() -> u64 {
    8000
}

fn default_xhs_jitter_ms() -> u64 {
    7000
}

impl Default for XhsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: default_xhs_url(),
            token_env: None,
            min_interval_ms: default_xhs_min_interval_ms(),
            jitter_ms: default_xhs_jitter_ms(),
        }
    }
}

fn default_xhs_url() -> String {
    "http://localhost:18060/mcp".into()
}

/// [web] HTTP/WS 服务配置。
/// 前后端分离基座：密钥只在本进程（.env），前端静态产物可挂任意托管；
/// bind/token/cors 全走配置，迁移到域名/服务器零代码改动。
#[derive(Debug, Deserialize, Clone)]
pub struct WebConfig {
    /// 监听地址（host:port），默认仅本机
    #[serde(default = "default_web_bind")]
    pub bind: String,
    /// 访问口令所在的环境变量名；真实值只放 .env。
    #[serde(default = "default_web_token_env")]
    pub token_env: String,
    /// 启动时从 token_env 解析出的访问口令，不参与 TOML 反序列化。
    /// REST/WS 均需携带（?token= 或 Authorization: Bearer）；空 = 不校验。
    #[serde(skip)]
    pub token: String,
    /// 全局同时在跑的 agent 轮数上限（跨会话；防多人同时对话拉高并发计费）
    #[serde(default = "default_web_max_concurrent")]
    pub max_concurrent: usize,
    /// 允许的跨域前端来源；同源页面依据 Host 自动放行，空 = 不放行额外跨域
    #[serde(default = "default_web_cors_origins")]
    pub cors_origins: Vec<String>,
    /// 正式 React 构建产物目录；不存在时退回 M0 冒烟页，便于纯后端测试。
    #[serde(default = "default_web_static_dir")]
    pub static_dir: String,
}

fn default_web_bind() -> String {
    "127.0.0.1:8080".into()
}

fn default_web_max_concurrent() -> usize {
    2
}

fn default_web_token_env() -> String {
    "WEB_TOKEN".into()
}

fn default_web_cors_origins() -> Vec<String> {
    vec![
        "http://localhost:5173".into(),
        "http://127.0.0.1:5173".into(),
    ]
}

fn default_web_static_dir() -> String {
    "web/dist".into()
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            bind: default_web_bind(),
            token_env: default_web_token_env(),
            token: String::new(),
            max_concurrent: default_web_max_concurrent(),
            cors_origins: default_web_cors_origins(),
            static_dir: default_web_static_dir(),
        }
    }
}

/// [auth] M2 认证配置。数据库 URL 不含密码；管理员初始密码以后只从 .env 注入。
#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct AuthConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_auth_database_url")]
    pub database_url: String,
    #[serde(default = "default_auth_cookie_name")]
    pub cookie_name: String,
    #[serde(default = "default_auth_session_ttl_hours")]
    pub session_ttl_hours: u64,
    /// 正式 HTTPS 环境必须为 true；本机 http 开发时为 false。
    #[serde(default)]
    pub secure_cookie: bool,
    /// 登录失败达到该次数后临时锁定账号。
    #[serde(default = "default_auth_max_failures")]
    pub max_login_failures: i64,
    /// 失败计数和锁定窗口（秒）。
    #[serde(default = "default_auth_lockout_secs")]
    pub lockout_secs: i64,
}

fn default_auth_database_url() -> String {
    "sqlite://data/cheaptrip.db".into()
}

fn default_auth_cookie_name() -> String {
    "cheaptrip_session".into()
}

fn default_auth_session_ttl_hours() -> u64 {
    24 * 7
}

fn default_auth_max_failures() -> i64 {
    5
}

fn default_auth_lockout_secs() -> i64 {
    300
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            database_url: default_auth_database_url(),
            cookie_name: default_auth_cookie_name(),
            session_ttl_hours: default_auth_session_ttl_hours(),
            secure_cookie: false,
            max_login_failures: default_auth_max_failures(),
            lockout_secs: default_auth_lockout_secs(),
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let text = std::fs::read_to_string("config.toml")
            .context("读取 config.toml 失败；首次运行请复制 config.example.toml 为 config.toml")?;
        let mut cfg: Self = toml::from_str(&text).context("解析 config.toml 失败")?;
        cfg.web.token = env::var(&cfg.web.token_env).unwrap_or_default();
        Ok(cfg)
    }

    pub fn api_key(&self) -> Result<String> {
        env::var(&self.llm.api_key_env)
            .with_context(|| format!("环境变量 {} 未设置", self.llm.api_key_env))
    }

    /// CJK 字体路径，从环境变量 MAP_FONT_PATH 读取
    pub fn font_path(&self) -> String {
        env::var("MAP_FONT_PATH").unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::Config;
    use crate::llm::provider::{Provider, ReasoningEffort};

    #[test]
    fn example_config_is_valid_and_safe_by_default() {
        let cfg: Config = toml::from_str(include_str!("../config.example.toml"))
            .expect("config.example.toml should remain parseable");

        assert!(!cfg.auth.enabled);
        assert_eq!(
            cfg.web.cors_origins,
            ["http://localhost:5173", "http://127.0.0.1:5173"]
        );
        assert_eq!(cfg.llm.provider, Provider::Auto);
        assert_eq!(cfg.llm.reasoning_effort, ReasoningEffort::Max);
    }

    #[test]
    fn old_config_defaults_provider_and_reasoning_effort() {
        let cfg: Config = toml::from_str(
            r#"
            [llm]
            base_url = "https://example.test"
            model = "GLM-5.3-Flash"
            api_key_env = "API_KEY"
            temperature = 0.7
            max_tokens = 100

            [cost]
            input_per_1m = 0.0
            output_per_1m = 0.0

            [session]
            "#,
        )
        .expect("old config remains valid");
        assert_eq!(cfg.llm.provider, Provider::Auto);
        assert_eq!(cfg.llm.reasoning_effort, ReasoningEffort::Max);
    }

    #[test]
    fn explicit_provider_and_reasoning_effort_deserialize() {
        let cfg: Config = toml::from_str(
            r#"
            [llm]
            base_url = "https://example.test"
            model = "gpt-4o"
            api_key_env = "API_KEY"
            temperature = 0.7
            max_tokens = 100
            provider = "openai_compatible"
            reasoning_effort = "low"

            [cost]
            input_per_1m = 0.0
            output_per_1m = 0.0

            [session]
            "#,
        )
        .expect("explicit provider settings deserialize");
        assert_eq!(cfg.llm.provider, Provider::OpenaiCompatible);
        assert_eq!(cfg.llm.reasoning_effort, ReasoningEffort::Low);
    }
}
