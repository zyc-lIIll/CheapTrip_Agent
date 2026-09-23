# 项目说明

## 项目介绍

本项目是 Rust 二进制 `cheaptrip`，用于实现「拾光者」旅游规划 Agent。
产品行为规格（人设、五阶段流程、工具、约束）见 `skills/` 下的 .md 文件和 `README.md`。

## 编码要求

- Rust edition 2024（见 `Cargo.toml`）；未固定工具链，使用本机已安装的 stable
- 不使用 `unsafe`
- 错误处理用 `Result`，不要用 `unwrap`/`expect` 掩盖错误

## 代码结构

脚手架已成型：`src/` 下分 `main.rs` / `config.rs` / `llm.rs` / `agent.rs` / `core.rs` / `session.rs` / `ui.rs` / `web/` / `tools/`（每工具一文件）。详细模块说明见 `README.md`。新增功能时遵循既有分层：工具加在 `src/tools/` 下并在 `mod.rs` 注册；TUI 实现并走 `core::Frontend` trait，Web 走 `src/web/` 服务层（复用 Core，不实现 Frontend）。

### 关键文件索引

| 改什么 | 改哪个文件 |
|--------|-----------|
| 人设/工具总览/约束 | `skills/system.md`（总则，常驻） |
| 各阶段详细指令 | `skills/phases/1.md`~`5.md`（按 session.phase 注入） |
| 技能文档按阶段挂载映射 | `src/agent.rs` 的 `skill_docs_for()`（代码写死，勿让 LLM 选） |
| 会话记忆笔记 | `sessions/{id}.md`（LLM 经 update_notes 工具整文件重写；`session.rs` 读写，删除联动） |
| 地图技能（何时调、地名策略、知识库） | `skills/map.md` |
| 行李/换乘/特殊约束 | `skills/special.md` |
| 酒店搜索策略 | `skills/hotel.md` |
| 景点/美食搜索策略 | `skills/discovery.md` |
| LLM 客户端（请求/流式/取消） | `src/llm.rs` |
| agent 循环 + event 推送 + 拦截 | `src/agent.rs` |
| 前后端接口 | `src/core.rs` |
| TUI 前端 | `src/ui.rs` |
| Web 服务层（M0） | `src/web/`（`mod.rs` 路由/口令鉴权/CORS/冒烟页、`rest.rs` 会话 CRUD、`state.rs` 会话→Core 句柄/事件扇出/并发闸门、`ws.rs` WS 推流 chat/stop、`media.rs` 产物图片下发、`test_page.html` M1 前替换） |
| 会话持久化 | `src/session.rs` |
| 配置加载 | `src/config.rs` |
| 新增工具 | `src/tools/xxx.rs` + `mod.rs` + `main.rs` |
| 地图生成（overview + city） | `src/tools/map/`（`overview.rs` 总览图 / `city.rs` 城市图 / `mod.rs` 共用工具） |
| part 几何刻画（聚类） | `src/tools/cluster_pois.rs`（纯几何，零网络请求） |
| 两点驾车查证（打车口径） | `src/tools/route_check.rs` |
| 12306 车次查询（免登录直查） | `src/tools/trains.rs`（leftTicket 接口；字段索引实测记录在头注释，改版需更新；超窗自动改查 today+10） |
| 知识库写入枢纽（接口预留） | `src/tools/knowledge.rs`（location/guide/hotel 三后端统一入口；**未启用**，注册锚点在 main.rs 注释，实现后放开） |
| 酒店评论爬虫（携程登录态方案） | `src/tools/hotel_reviews.rs` + `scripts/hotel_crawl/`（login.mjs 扫码存登录态、logout.mjs 登出、crawl.mjs 爬取；`.ctrip-state.json` 含 cookies 绝不入库；输出 `hotels/{sid}/hotel_{n}/`，删除联动清理；**先筛后爬**：只核验定稿候选、每家最多 5 图、单次规划 ≤2~3 家——纪律在 hotel.md） |
| 小红书只读工具（MCP 客户端） | `src/tools/xhs.rs`（search_xhs / read_xhs_note，经本机 xiaohongshu-mcp 服务） |
| 小红书登录/状态辅助脚本 | `scripts/xhs-login.sh`（二维码落当前目录；check/logout/token 子命令） |
| 环境自检/配置向导 | `scripts/setup.sh`（--check 只体检；小红书步骤标【选装】） |
| 模型/费用/会话参数 | 本地 `config.toml`（从入库的 `config.example.toml` 复制；本地文件不提交） |
| 密钥/环境变量 | `.env`（从 `.env.example` 复制） |

### 数据流

```
用户输入 → Core.submit() → agent.run() → LLM chat_stream()
  ↓ SSE 流式 → AgentEvent::{Content, Reasoning, ToolCallDelta, Usage, Done}
  ↓ 工具调用 → Tool::execute() → AgentEvent::{ToolCall, ToolResult}
  ↓ 阶段切换 → set_phase 拦截 → AgentEvent::PhaseChange
  ↓ 完成 → AgentEvent::Done → session.save()
```

Web 链路（M0）：浏览器 → axum（`src/web/`）→ `AppState.ensure_core(sid)` 按会话 spawn Core；
WS 收 `{"type":"chat","text"}` → 全局并发闸门（`[web].max_concurrent`）→ `CoreHandle.chat()`；
事件经 broadcast 扇出到各 WS 连接（`{"type":"event","event":<AgentEvent 原样>}`），forwarder
见一轮终点事件（Done/Error）归还并发许可；`{"type":"stop"}` 取登记的 CancellationToken。

### `AgentEvent` 变体

| 变体 | 说明 |
|------|------|
| `Step { n }` | 第 n 次 LLM 调用开始 |
| `Content(String)` | 正文增量 |
| `Reasoning(String)` | 推理增量（GLM reasoning_content） |
| `ToolCall { name, args }` | 工具调用开始 |
| `ToolResult(String)` | 工具返回 |
| `PhaseChange { phase }` | 阶段切换 |
| `Usage(Usage)` | token 用量更新（UI 算费用） |
| `Done(String)` | 一轮结束 |
| `Error(String)` | 错误/取消 |

## 工作范式（协作模式）

> 本项目的协作流程约定，开发时务必遵守。

1. **分步推进，不一次性全做**：用户说「接着往下做」后，**先通过问答把方案确认清楚**（接口、数据流、取舍点），再分小步实现；每步编译/测试通过再进入下一步，不要一次性把代码+文档+测试全推完。
2. **实现前先调研**：涉及外部 API / 第三方 crate / MCP，先到 web / GitHub / crates.io / 官方文档查清能力边界与已知坑（如中国坐标偏移、镜像 403），再定方案；调研结论要先同步给用户。
3. **每步三连验证**：`cargo check` + `cargo fmt --check` + `cargo clippy` 全过才算一步完成；能实跑的工具加 `#[ignore]` 联网测试（`cargo test <name> -- --ignored --nocapture`）。
4. **用 todo 列表跟踪**：开干前列 todo，实时更新状态，完成一步再标下一步 in_progress。
5. **设计取舍优先问用户**：接口形态、依赖增减、合规风险等决策点用问答确认，不擅自定。
6. **测试留窗口不删**：测试函数写好后永久保留在 `tests` 模块里，不要用完就删。每个工具有独立的 `#[ignore]` 测试，用 `cargo test <name> -- --ignored --nocapture` 运行。不需要某个测试时由用户明确说删才删。
7. **每步做完及时更新 README**：代码改动完成后，同步更新 `README.md` 的进度、工具列表、模块结构、配置说明等，保持文档和代码一致。
8. **用户逐点说要微调的地方**：用户会逐条描述要改的点，agent 逐条修改、测试、确认，不要一次性改完所有点。
9. **skill 文档可迭代**：`skills/` 下的 .md 文件是 system prompt 的一部分，用户可自行编辑迭代，改了重新编译生效，不需改代码。

### 已确立的设计约定

- **地图工具数据 vs 渲染分离**：LLM 只负责「画什么」（何时调用、提供坐标/归属/交通方式），工具内部确定性决定「怎么画」。渲染逻辑**代码写死**，不让 LLM 决定渲染细节。
- **地图分两种**：总览图（overview）= 白底 part 化模式图（意会圈+辐射线+大交通），不用真实瓦片；城市详细图（city）= 高德瓦片底图 + 真实路线（无缩略图，plan_view 只避让图例带与边缘）。两者拆成独立工具。
- **part 划分数据支撑**：`cluster_pois` 两段式防桥接聚类（T=median(nn)×2.5 夹限 5~25km，散点只挂靠不入核，零散景点当不了桥）+ LLM 先验（`anchors`/`min_regions`，不符预期带参重调不硬掰）+ `route_check` 打车口径查证；多归属不让用户选，LLM 按攻略定一个、展示提一嘴「介于X与Y之间」。
- **overview part 化接口**：`hubs[]`（区中心）+ `points[]`（type 三样式 + 可选 `hubs[]` 显式归属）+ `routes[]`（大交通）；意会圈半径 = 区内点（d≤20km）最远距离×1.2（下限 4km 上限 60km）由代码定；hubs 可空——空时为纯点位位置关系图（阶段1 候选展示，不画圈不画交通，图例按实际内容裁剪）；标签重叠自动打包：`label_rect` 与绘制同源算矩形、相交者经 `single_link_clusters` 链式归组，组画概述（含 hub 以 hub 名计数）+ 明细进图例 + 每组追加聚焦放大图 `overview_{n}_c{k}.png`（hubs 空/禁打包渲染，上限 3 张，同 city 簇图惯例）；网格步长经 `nice_step_for_px` 按统一像素间距选档，经纬线密度观感一致，标注小数位随步长走。
- **重叠路线偏移**：总览图同起终点（不分方向）的多条路线，沿垂直于连线方向逐条偏移错开，先统计条数再按步长分配偏移。
- **CJK 字体不入仓**：路径通过 `.env` 的 `MAP_FONT_PATH` 环境变量配置，运行时从 `Config::font_path()` 读取。
- **图片产物落地**：存 `maps/{session_id}/{type}_{n}.png`；`n` 从目录已有图片的最大编号续接（`scan_max_index`，重进会话/重启不覆盖旧图），主图失败不消耗编号；TUI 阶段工具返回路径文本，Web/手机端再考虑内联。测试垃圾（`maps/test`、`maps/probe`）用 `cargo run -- clean-test` 清理。
- **高德 zoom 语义偏置**：实测请求 zoom=z 实际渲染对应瓦片 z+1（`AMAP_ZOOM_BIAS`）。zoom 选择与中心换算必须统一经 `plan_view`，勿绕过；视图规划按「POI+路线 polyline 并集」避让图例带/边缘。
- **换乘路线按最优方案全量绘制**（含步行接驳段），整条模式按方案内容判定（`classify_transit_plan`：公共交通/打车/步行骑行），颜色与图例经 `mode_color_hex`/`mode_display` 三大类呈现，具体线路由 LLM 文字说明。
- **图例单源**：条目在 `draw_city_map`/`render_overview` 一次构建，`flow_legend_band` 实测高度传给视图规划预留，绘制与测高共用 `map/mod.rs` 的折行逻辑；改布局只动 `LEGEND_*` 常量。
- **高德请求纪律**：所有高德 API 调用前必须 `RateLimiter::wait()`（共享 400ms 限流，tools/mod.rs）；HTTP 客户端带超时并复用，不每次新建；路线合并组数>4 在发请求前预检拒绝；静态图 paths 参数按渲染 zoom 做 DP 抽稀（约 1px 容差）+ 6KB URL 预算兜底，防 414。
- **LLM 流式**：禁用总超时（会掐断长流）；`read_timeout` 为空闲检测，可在 `config.toml [llm]` 配置；起始断流由 agent 重试（退避 1/2/3s，可打断）；max_tokens 截断——纯正文从断点续写（≤3 次），截断落在工具调用参数里则丢弃半截参数、请模型重发精简版（≤3 次，重试用尽报清晰错误而非 JSON EOF）。
- **小红书只读接入**：经本机 xiaohongshu-mcp 服务（MCP StreamableHTTP，手写 JSON-RPC 客户端零依赖，`src/tools/xhs.rs`）；`[xhs].enabled` 才注册工具与挂载 xhs.md（阶段 2/3/4）；服务未启动/未登录报错不阻塞、退回 search_web；**写操作仅放行精读后 5% 概率自动点赞**（like_feed 幂等，2026-09-10 用户决策），发布/评论/关注等其余写操作一律不接；session 失效自动重握手一次；**拟人防风控**（2026-09-10，账号被风控后的对策）：MCP 调用随机变速限流（min_interval_ms+jitter_ms 可配）、搜索结果只透传头部 3~5 篇、评论默认随机读 3~8 条（封顶 10），xhs.md 写死搜索词多样化/总量克制/评论少不深读纪律
- **Session 与图片强绑定**：删除会话时联动删除 `maps/{session_id}/` 目录。
- **Web M0 基座**（2026-09-11 落地）：axum 0.8 包 Core（`src/web/`），REST 会话 CRUD + WS 推流 + /media 图片下发；**AgentEvent 本体零改动**（WS 外层包 `{"type":"event","event":...}`，连上先补 history）；简单口令鉴权（`[web].token`，?token= 或 Bearer，REST/WS/media 全覆盖；空=不校验仅本机）；**迁移纪律：URL/CORS/口令全走 config.toml `[web]`，代码不硬编码域名**；并发闸门 `[web].max_concurrent`（默认 2）跨会话限流、许可由 forwarder 在一轮终点（Done/Error 恰好一个）归还，单会话同时只允许一轮；sid 白名单 `[A-Za-z0-9_-]`，media 相对路径逐段校验 + canonicalize 双保险防穿越；`Session::list()` 对并发增删容错（单条失败跳过）；已知限制：流式中途断线重连拿不到未落盘增量（一轮结束才 save）
- **Token 统计持久化**：`Session.usage` 每轮累加后存盘，加载旧会话恢复；UI 通过 `AgentEvent::Usage` 实时刷新。
- **阶段0 + 五阶段流程**：0种草闲聊（新会话默认，唠嗑定目的地，业务工具仅 search_web/search_xhs 画饼吸睛） 1信息采集 2大局规划（geocode+聚类刻画part+游玩大方向+全程总览图） 3逐part确定（景点+行程+住宿一次过，按part循环，不必最终敲定但必须真方案） 4整体调整+预算（换酒店/景点/交通必须按对应skill查证，hotel最重要） 5完整攻略。
- **新密钥**一律放 `.env` + 在 `SECRETS.md` 登记位置 + `.gitignore` 忽略相关文件。

## 常用命令

```sh
cargo check            # 快速类型/编译检查
cargo build            # 构建
cargo run               # 运行（CLI 冒烟）
cargo run -- TUI        # TUI 模式
cargo run -- web        # Web 模式（M0：REST+WS 基座 + 内置冒烟页；口令/演示见 README「Web 模式」）
cargo fmt               # 格式化（检查用 cargo fmt --check）
cargo clippy            # lint
cargo test              # 测试
cargo test <name> -- --ignored --nocapture  # 跑联网实跑测试（地图/天气/geocode/小红书等）
cargo run -- clean-test # 清空测试地图垃圾（maps/test、maps/probe、hotels/test、hotels/probe）
cargo run -- hotel login|logout|on|off|status  # 携程登录态/爬虫开关管理
cargo run -- xhs on|off|status  # 小红书工具开关管理（重启生效）
scripts/setup.sh [--check]  # 快速配置向导（Rust 工具链/.env 密钥/CJK 字体/编译/小红书【选装】）
scripts/xhs-login.sh check|logout|token  # 小红书服务连通/登录状态；退出账号；启用-更换-关闭服务门禁（详见 README「小红书接入」）
```

## 安全要求

不要在项目中透露 API key、账号密码等敏感信息；如确需放入某处，必须告知用户具体位置。
`.gitignore` 当前忽略 `/target`、`/config.toml`、`/.env`、`/SECRETS.md`、`/sessions/`、`/maps/`、`/hotels/`、`/.cargo/`、`.toggles.json`、`scripts/hotel_crawl/`（node_modules、out、.ctrip-state.json）、`/docs/`；公开默认配置只维护 `config.example.toml`，任何新增的本地配置或密钥文件须显式加入 `.gitignore`，避免误提交。
`scripts/hotel_crawl/.ctrip-state.json` 含携程账号会话 cookies（等同密码），绝不入库；登出 `cargo run -- hotel logout`。

## 作业固定要求

- **R1**: 核心逻辑用 Rust 实现。
- **R2**: 提供用户交互界面（Web / CLI / 桌面 / 手机）。
- **R3**: 模型与参数可配置，不写死。
- **R4**: 实时展示进度，支持打断。
- **R5**: 上下文历史管理，可保存/加载。
- **R6**: 统计 Token 用量与费用。
