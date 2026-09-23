mod agent;
mod config;
mod core;
mod llm;
mod session;
mod toggles;
mod tools;
mod ui;
mod web;

use anyhow::{Context, Result};
use std::path::Path;
use std::sync::Arc;

use crate::core::{Core, Frontend};

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt::init();

    // 子命令：clean-test —— 清空测试/探针会话的地图垃圾文件后退出
    // （联网测试与诊断探针固定用 "test"/"probe" 两个 session id）
    if std::env::args().nth(1).as_deref() == Some("clean-test") {
        return clean_test_maps();
    }
    // 子命令：hotel / xhs —— 登录/登出/开关/状态管理
    match std::env::args().nth(1).as_deref() {
        Some("hotel") => return hotel_manage(std::env::args().nth(2).as_deref()),
        Some("xhs") => return xhs_manage(std::env::args().nth(2).as_deref()),
        Some("admin") => return admin_manage().await,
        _ => {}
    }

    let mut cfg = config::Config::load()?;
    // xhs 运行时开关：toggles 关闭即等效于 config 关闭（agent 侧零改动）
    if !toggles::Toggles::load().xhs {
        cfg.xhs.enabled = false;
    }

    // 启动时清理：只保留最近若干会话（cli 永久保留）+ 裁剪各会话消息数
    session::Session::prune(cfg.session.max_sessions)?;
    session::Session::trim_all(cfg.session.max_messages)?;
    let client = llm::Client::new(&cfg)?;

    // 构建工具列表（各工具所需的 API key 从环境变量读取）
    let exa_key = std::env::var("EXA_API_KEY").context("EXA_API_KEY 未设置")?;
    let amap_key = std::env::var("AMAP_API_KEY").context("AMAP_API_KEY 未设置")?;
    let font_path = cfg.font_path();
    // 高德 API 全局限流：所有高德请求（天气/geocode/路径/静态图）共享，相邻间隔 ≥400ms ≈ 2.5 QPS
    let amap_limiter = tools::RateLimiter::new(400);
    // 小红书工具：仅当 config.toml [xhs].enabled = true 时注册
    // （需本机运行 xiaohongshu-mcp 服务并扫码登录；token 走环境变量，见 .env.example）
    let xhs_client = if cfg.xhs.enabled {
        let token = cfg
            .xhs
            .token_env
            .as_deref()
            .filter(|s| !s.is_empty())
            .and_then(|name| std::env::var(name).ok())
            .filter(|s| !s.is_empty());
        Some(tools::xhs::XhsClient::new(
            cfg.xhs.url.clone(),
            token,
            cfg.xhs.min_interval_ms,
            cfg.xhs.jitter_ms,
        ))
    } else {
        None
    };
    let build_tools = move |session_id: &str| {
        let mut list: Vec<Box<tools::DynTool>> = vec![
            Box::new(tools::GetWeather::new(
                amap_key.clone(),
                amap_limiter.clone(),
            )) as Box<tools::DynTool>,
            Box::new(tools::Geocode::new(amap_key.clone(), amap_limiter.clone())),
            Box::new(tools::RouteCheck::new(
                amap_key.clone(),
                amap_limiter.clone(),
            )),
            Box::new(tools::SearchWeb::new(exa_key.clone())),
            Box::new(tools::SearchTrains::new()),
            Box::new(tools::ClusterPois),
            Box::new(tools::GenerateMap::new(
                font_path.clone(),
                session_id.to_string(),
            )),
            Box::new(tools::GenerateCityMap::new(
                amap_key.clone(),
                font_path.clone(),
                session_id.to_string(),
                amap_limiter.clone(),
            )),
            Box::new(tools::SetPhase),
            Box::new(tools::SetMode),
            Box::new(tools::UpdateNotes::new(session_id.to_string())),
        ];
        // 酒店评论爬虫：环境就绪（脚本+登录态+playwright）才注册，避免 LLM 调用必失败
        let hotel_tool = tools::HotelReviews::new(session_id.to_string());
        if toggles::Toggles::load().hotel_crawler && hotel_tool.env_ready().is_ok() {
            list.push(Box::new(hotel_tool));
        }
        // 知识库枢纽工具（update_knowledge）：接口已预留、写入逻辑未实现。
        // 实现后放开下面注释即可注册（详见 src/tools/knowledge.rs 头注释）。
        // list.push(Box::new(tools::knowledge::KnowledgeHub));
        if let Some(xc) = &xhs_client {
            list.push(Box::new(tools::SearchXhs::new(xc.clone())));
            list.push(Box::new(tools::ReadXhsNote::new(xc.clone())));
        }
        list
    };

    // Web 服务模式：`./trip web local`（M0：REST + WS 基座；TUI/CLI 行为不变）
    if std::env::args().nth(1).as_deref() == Some("web") {
        return web::run(cfg, client, Arc::new(build_tools)).await;
    }

    // TUI 模式（`./trip tui`，与 web 同款子命令格式）；否则走 CLI 临时对话。
    // 新增 Web/手机前端：在此分支选用对应 Frontend 实现即可。
    let tui_mode = matches!(
        std::env::args().nth(1).as_deref(),
        Some("TUI") | Some("tui")
    );
    if tui_mode {
        // TUI：循环「选会话 → 对话 → Ctrl-L 切回选择页」
        loop {
            let session = ui::pick_session().await?;
            let history = session.messages.clone();
            let usage = session.usage.clone();
            let phase = session.phase;
            let agent = agent::Agent::new(client.clone(), &cfg, build_tools(&session.id));
            let core = Core::spawn(agent, session);
            let switch = ui::Tui {
                model: cfg.llm.model.clone(),
                history,
                usage,
                phase,
                cost_per_1m: (cfg.cost.input_per_1m, cfg.cost.output_per_1m),
            }
            .run(core)
            .await?;
            if !switch {
                break;
            }
        }
        Ok(())
    } else {
        // CLI：固定 id "cli" 当临时对话（覆盖式）
        let session = session::Session::new("cli".into());
        let agent = agent::Agent::new(client, &cfg, build_tools("cli"));
        let core = Core::spawn(agent, session);
        run_cli(core, cfg.llm.model.clone()).await
    }
}

async fn admin_manage() -> Result<()> {
    use web::auth::{AuthBackend, password, repository};

    let mut args = std::env::args().skip(2);
    let action = args.next().as_deref().unwrap_or("help").to_owned();
    let cfg = config::Config::load()?;
    let backend = AuthBackend::connect(&cfg.auth).await?;
    match action.as_str() {
        "init" => {
            let username = args
                .next()
                .context("用法：./trip admin init <用户名> --password-stdin")?;
            let password = read_password_input(&mut args, "请输入管理员密码: ")?;
            if repository::count_admins(&backend.pool).await? > 0 {
                anyhow::bail!("管理员已存在；不能重复初始化");
            }
            let hash = password::hash_password(&password)?;
            let user = repository::create_user(
                &backend.pool,
                &username,
                &hash,
                web::auth::model::Role::Admin,
            )
            .await?;
            println!(
                "管理员已创建：{}（数据库：{}）",
                user.username, cfg.auth.database_url
            );
        }
        "list" => {
            for user in repository::list_users(&backend.pool).await? {
                println!(
                    "{}\trole={}\tenabled={}\tmust_change_password={}",
                    user.username,
                    user.role.as_str(),
                    user.enabled,
                    user.must_change_password
                );
            }
        }
        "migrate-sessions" => {
            let username = args
                .next()
                .context("用法：./trip admin migrate-sessions <管理员用户名>")?;
            anyhow::ensure!(args.next().is_none(), "migrate-sessions 不接受额外参数");
            let user = repository::find_by_username(&backend.pool, &username)
                .await?
                .context("指定管理员不存在")?;
            anyhow::ensure!(
                user.role == web::auth::model::Role::Admin,
                "只能把旧会话迁移给管理员"
            );
            let count = session::Session::assign_unowned_to(user.id)?;
            println!("已将 {count} 个未归属会话迁移给管理员 {}。", user.username);
        }
        "reset-password" => {
            let username = args
                .next()
                .context("用法：./trip admin reset-password <用户名> --password-stdin")?;
            let password = read_password_input(&mut args, "请输入新密码: ")?;
            let hash = password::hash_password(&password)?;
            let user = repository::reset_password(&backend.pool, &username, &hash).await?;
            println!(
                "已为 {} 设置一次性新密码；首次登录必须修改。",
                user.username
            );
        }
        _ => {
            anyhow::bail!("用法：./trip admin init|list|reset-password|migrate-sessions ...")
        }
    }
    Ok(())
}

fn read_password_input(args: &mut impl Iterator<Item = String>, prompt: &str) -> Result<String> {
    if args.next().as_deref() == Some("--password-stdin") {
        if args.next().is_some() {
            anyhow::bail!("--password-stdin 后不能再有其他参数");
        }
        return read_password_stdin();
    }
    anyhow::ensure!(args.next().is_none(), "未知参数；可使用 --password-stdin");
    use std::io::{IsTerminal, Write};
    anyhow::ensure!(
        std::io::stdin().is_terminal(),
        "非交互环境请使用 --password-stdin"
    );

    crossterm::terminal::enable_raw_mode().context("启用密码输入模式失败")?;
    print!("{prompt}");
    std::io::stdout().flush().context("刷新密码提示失败")?;
    let result = (|| -> Result<String> {
        let mut password = String::new();
        loop {
            if let crossterm::event::Event::Key(key) =
                crossterm::event::read().context("读取密码输入失败")?
            {
                match key.code {
                    crossterm::event::KeyCode::Enter => break,
                    crossterm::event::KeyCode::Backspace => {
                        password.pop();
                    }
                    crossterm::event::KeyCode::Char(ch)
                        if key.modifiers == crossterm::event::KeyModifiers::NONE =>
                    {
                        password.push(ch);
                    }
                    crossterm::event::KeyCode::Esc => anyhow::bail!("已取消密码输入"),
                    _ => {}
                }
            }
        }
        Ok(password)
    })();
    crossterm::terminal::disable_raw_mode().context("恢复终端输入模式失败")?;
    println!();
    result
}

fn read_password_stdin() -> Result<String> {
    use std::io::BufRead;
    let mut password = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut password)
        .context("读取标准输入密码失败")?;
    let password = password.trim_end_matches(['\r', '\n']).to_owned();
    if password.chars().count() < 8 {
        anyhow::bail!("密码至少需要 8 个字符");
    }
    Ok(password)
}

/// hotel 子命令管理：login | logout | on | off | status。
fn hotel_manage(sub: Option<&str>) -> Result<()> {
    const DIR: &str = "scripts/hotel_crawl";
    const STATE: &str = "scripts/hotel_crawl/.ctrip-state.json";
    match sub {
        Some("login") => {
            if !Path::new(&format!("{DIR}/crawl.mjs")).exists() {
                anyhow::bail!("爬虫脚本缺失（{DIR}/crawl.mjs），请先拉取完整仓库");
            }
            if !Path::new(&format!("{DIR}/node_modules")).exists() {
                println!(
                    "playwright 未安装，先执行：cd {DIR} && npm i playwright && npx playwright install chromium"
                );
                anyhow::bail!("缺少依赖");
            }
            let st = std::process::Command::new("node")
                .arg("login.mjs")
                .current_dir(DIR)
                .status()
                .context("启动 node 失败（检查 node 是否可用）")?;
            if !st.success() {
                anyhow::bail!("登录未完成（可重新运行 ./trip hotel login）");
            }
            println!("登录态已就绪，爬虫可用（开关状态见 ./trip hotel status）");
        }
        Some("logout") => {
            let f = Path::new(STATE);
            if f.exists() {
                std::fs::remove_file(f).with_context(|| format!("删除 {STATE} 失败"))?;
                println!("已删除携程登录态（下次爬取前需 ./trip hotel login）");
            } else {
                println!("本就无登录态，无需登出");
            }
        }
        Some("on") | Some("off") => {
            let val = sub == Some("on");
            let t = toggles::Toggles::set("hotel", val)?;
            println!(
                "酒店爬虫已{}（toggles.hotel_crawler = {}）；登录态：{}",
                if val { "启用" } else { "禁用" },
                t.hotel_crawler,
                if Path::new(STATE).exists() {
                    "在"
                } else {
                    "无（需 ./trip hotel login）"
                }
            );
        }
        Some("status") | None => {
            let t = toggles::Toggles::load();
            println!(
                "开关        : {}",
                if t.hotel_crawler { "启用" } else { "禁用" }
            );
            println!(
                "爬虫脚本    : {}",
                if Path::new(&format!("{DIR}/crawl.mjs")).exists() {
                    "在"
                } else {
                    "缺"
                }
            );
            println!(
                "playwright  : {}",
                if Path::new(&format!("{DIR}/node_modules")).exists() {
                    "已安装"
                } else {
                    "未安装"
                }
            );
            println!(
                "登录态      : {}",
                if Path::new(STATE).exists() {
                    "在"
                } else {
                    "无（./trip hotel login）"
                }
            );
            let ready = t.hotel_crawler
                && Path::new(&format!("{DIR}/crawl.mjs")).exists()
                && Path::new(&format!("{DIR}/node_modules")).exists()
                && Path::new(STATE).exists();
            println!(
                "综合        : {}",
                if ready { "可用 ✓" } else { "不可用 ✗" }
            );
        }
        other => anyhow::bail!(
            "未知子命令 {:?}（可用：login | logout | on | off | status）",
            other
        ),
    }
    Ok(())
}

/// xhs 子命令管理：login | logout | check | on | off | status。
fn xhs_manage(sub: Option<&str>) -> Result<()> {
    match sub {
        // 登录/登出/连通性检查：委托 scripts/xhs-login.sh（MCP 容器侧操作），输出直通终端
        Some(op @ ("login" | "logout" | "check")) => {
            let script = Path::new("scripts/xhs-login.sh");
            if !script.exists() {
                anyhow::bail!("缺少 scripts/xhs-login.sh");
            }
            let mut cmd = std::process::Command::new("bash");
            cmd.arg(script);
            if op != "login" {
                cmd.arg(op); // 脚本约定：无参=登录
            }
            let st = cmd
                .status()
                .context("执行 scripts/xhs-login.sh 失败（检查 bash 可用性）")?;
            if !st.success() {
                anyhow::bail!("xhs {op} 未成功（见上方脚本输出）");
            }
        }
        Some("on") | Some("off") => {
            let val = sub == Some("on");
            let t = toggles::Toggles::set("xhs", val)?;
            let cfg_enabled = config::Config::load()
                .map(|c| c.xhs.enabled)
                .unwrap_or(false);
            println!(
                "小红书工具已{}（toggles.xhs = {}）",
                if val { "启用" } else { "禁用" },
                t.xhs
            );
            if val && !cfg_enabled {
                println!("注意：config.toml [xhs] enabled = false，需改为 true 才会注册工具");
            }
            println!("重启 cheaptrip 后生效");
        }
        Some("status") | None => {
            let t = toggles::Toggles::load();
            let cfg = config::Config::load()?;
            let effective = t.xhs && cfg.xhs.enabled;
            println!(
                "config 开关 : {}",
                if cfg.xhs.enabled { "启用" } else { "禁用" }
            );
            println!("运行时开关  : {}", if t.xhs { "启用" } else { "禁用" });
            println!(
                "综合        : {}",
                if effective {
                    "已启用（重启后生效）✓"
                } else {
                    "已禁用 ✗"
                }
            );
            println!("MCP 端点    : {}", cfg.xhs.url);
            println!("登录/连通   : ./trip xhs login | check");
        }
        other => anyhow::bail!(
            "未知子命令 {:?}（可用：login | logout | check | on | off | status）",
            other
        ),
    }
    Ok(())
}

/// 清理测试产物：maps/{test,probe} 与 hotels/{test,probe}（联网测试/诊断探针的固定落盘位置）。
fn clean_test_maps() -> Result<()> {
    for (base, sid) in [
        ("maps", "test"),
        ("maps", "probe"),
        ("hotels", "test"),
        ("hotels", "probe"),
    ] {
        let dir = std::path::Path::new(base).join(sid);
        if dir.exists() {
            let n = std::fs::read_dir(&dir)?.count();
            std::fs::remove_dir_all(&dir)
                .with_context(|| format!("删除 {} 失败", dir.display()))?;
            println!("已删除 {}（{n} 个文件）", dir.display());
        } else {
            println!("{} 不存在，跳过", dir.display());
        }
    }
    Ok(())
}

/// CLI 冒烟模式：自动发一条消息 + 2.5s 后取消，验证事件链路与打断。
async fn run_cli(core: Core, model: String) -> Result<()> {
    use tokio_util::sync::CancellationToken;

    println!("=== CLI 冒烟模式（model={model}）2.5s 后取消 ===");
    let (tx_user, mut rx_event) = core.into_parts();
    let cancel = CancellationToken::new();
    let fire = cancel.clone();
    tx_user
        .send((
            "请详细介绍北京从周口店到现代的完整历史，至少两千字，慢慢讲。".into(),
            cancel,
        ))
        .await?;

    let handle = tokio::spawn(async move {
        while let Some(ev) = rx_event.recv().await {
            ev.print();
        }
    });

    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
    println!("\n\n>> 触发取消");
    fire.cancel();
    let _ = handle.await;
    Ok(())
}
