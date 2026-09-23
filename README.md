# cheaptrip — 拾光者旅游规划 Agent

cheaptrip 是一个 Rust 实现的旅游规划 Agent。它会把种草闲聊、信息采集、路线规划、景点与住宿查证、预算调整和最终攻略串成一段可追问的对话。

主要能力：

- 流式输出，显示正文、思考、工具进度、阶段和 token 用量；生成中可以停止。
- 快捷模式和完整攻略模式，适合从单个问题到完整行程的不同深度。
- 景点、美食、天气、地理编码、驾车距离、12306 车次、地图和酒店评论工具。
- TUI 与响应式 Web 界面；会话、记忆笔记、费用统计和图片产物可以持久化。
- GLM 与通用 OpenAI-compatible 服务商可配置，思考强度和费用单价不写死在代码里。

## 三步开始

### 1. 获取项目并进入目录

```sh
git clone <仓库地址> cheaptrip
cd cheaptrip
```

### 2. 运行配置向导

```sh
./trip setup
```

向导会检查 Linux/WSL、Bash、Rust stable、Node.js 20+、npm 和 curl，从模板创建本机配置，隐藏读取三个必需 API key，并安装/构建 Web 与 Rust。已有 `config.toml` 或 `.env` 不会被覆盖；`.env` 会按 600 权限保存。

酒店 Playwright、小红书 MCP、CJK 字体和固定域名 Tunnel 都是选装项，跳过不影响核心配置。向导完成后的下一步只有三种生产启动方式：

```text
./trip web local
./trip web random
./trip web domain
```

不想修改本机文件时使用严格只读体检：

```sh
./trip setup check
```

体检不会创建配置、安装依赖、构建产物、修改容器或登录账号；必需项失败会返回非零。

### 3. 启动并新建会话

本机使用：

```sh
./trip web local
```

浏览器打开 `http://127.0.0.1:8080`。新建会话时选择“快捷模式”或“完整攻略”，之后该会话的顶层模式保持不变。

## 统一命令

根目录的 `./trip` 是所有日常操作入口。运行 `./trip help` 可查看当前帮助。

| 命令 | 用途 |
|---|---|
| `./trip setup` | 交互式配置、安装和构建 |
| `./trip setup check` | 严格只读环境体检 |
| `./trip web local` | 构建前端并在本机 8080 启动 |
| `./trip web random` | 构建前端并通过随机 Quick Tunnel 公网访问 |
| `./trip web domain` | 构建前端并连接本机已有固定域名 Tunnel |
| `./trip web dev` | 同时启动 Rust 后端和 Vite 开发前端 |
| `./trip tui` | 启动终端界面 |
| `./trip cli` | 运行一次 CLI 冒烟对话 |
| `./trip verify` | 执行 Rust 与 Web 的格式、编译、测试和构建检查 |
| `./trip clean test` | 清理地图和酒店测试产物 |
| `./trip hotel ...` | 管理携程酒店评论选装集成 |
| `./trip xhs ...` | 管理小红书 MCP 选装集成 |
| `./trip admin ...` | 管理 Web 账号和管理员设置 |

应用需要的依赖、配置和密钥均由 `./trip setup` 处理。不要把真实值写进模板、代码或提交。

## 快捷模式与完整攻略

### 快捷模式

快捷模式用于一个明确任务：例如种草推荐、排程建议、地图问题、车次查询、小红书搜索或携程候选核验。Agent 会根据当前问题选择相应的快捷子模式和最小工具集合，响应更短，适合快速问答和局部查证。

### 完整攻略

完整攻略按五个业务阶段推进：

1. 信息采集：目的地、日期、出发地、人数、预算、交通、行李和全局特殊要求。
2. 大局规划：查证候选、地理编码、按交通便利区划分 part，并生成总览图。
3. 逐 part 确定：每个 part 给出景点、日程和住宿的真实方案，再按反馈调整。
4. 整体调整与预算：统筹大交通、part 顺序、酒店、门票和预算。
5. 完整攻略：校验车次与预约，重新生成地图，输出攻略和执行清单。

完整攻略会按阶段挂载对应技能文档和工具；地图、住宿、换乘等需要查证的内容不会只凭模型记忆编造。

两种模式都支持流式输出、停止生成、会话历史、图片和 token 统计。新会话的第一条用户消息由服务端本地生成标题，最多 15 个 Unicode 字符，不额外调用模型；快捷与完整模式都自动命名。模式是在新建会话时选择的，聊天过程中不会自动切换顶层模式。

## Web 的四种启动方式

### 本机生产模式

```sh
./trip web local
```

先构建 `web/`，再启动 Rust Web 服务，监听配置中的地址（默认 `127.0.0.1:8080`）。本模式不要求 `WEB_TOKEN`，适合本机使用；按 Ctrl-C 会停止服务。

### 随机公网 Demo

```sh
./trip web random
```

需要本机已有 `cloudflared` 和非空 `WEB_TOKEN`。脚本会创建一次性的 Quick Tunnel，打印随机 `trycloudflare.com` 地址；地址每次启动都会变化，电脑或服务停止后链接失效。访问口令不会由脚本打印，临时分享时不要把口令写入截图、日志或公开页面。

### 固定域名模式

```sh
./trip web domain
```

需要本机 `.env` 中已有 `WEB_TOKEN`、`TRIP_DOMAIN` 和 `TRIP_TUNNEL_CONFIG`，以及可用的 Named Tunnel 配置。程序只做配置校验和连接，不创建 Cloudflare 资源；固定域名只写在你自己的 `.env`，不要提交。

### 开发模式

```sh
./trip web dev
```

同时启动 Rust 后端和 Vite 开发前端，浏览器地址通常由 Vite 显示（默认 5173）。前端把 `/api`、`/ws` 和 `/media` 代理到 Rust 服务。按 Ctrl-C 会同时停止两个进程。

生产模式会优先托管 `web/dist` 中的 React 构建产物；产物不存在时才使用内置的简单页面。四种模式都使用 `config.toml` 的 `[web]` 配置，代码不写死公网域名。

## Web 登录与权限

默认 `[auth].enabled = false`，只使用 `WEB_TOKEN` 口令（为空时不校验，仅适合本机）。需要多用户账号时：

1. 使用 `./trip admin init <用户名>` 创建首个管理员，密码在终端隐藏输入；非交互环境可使用 `--password-stdin`。
2. 在本机 `config.toml` 将 `[auth].enabled` 改为 `true`，然后重启 Web 服务。
3. 在 `/login` 登录；管理员可在设置页管理普通用户、模型参数、费用、集成开关和密钥状态。

常用管理命令：

```sh
./trip admin list
./trip admin reset-password <用户名>
./trip admin migrate-sessions <管理员用户名>
```

管理员设置只返回密钥“已配置/未配置”，不会返回真实 API key。启用账号认证后，`WEB_TOKEN` 不能绕过账号权限；正式 HTTPS 部署应把 `secure_cookie` 设为 `true`，并使用反向代理和严格的跨域来源。

## 选装集成

### CJK 字体

地图绘制中文标签需要本机 CJK 字体。把字体文件路径填入 `.env` 的 `MAP_FONT_PATH` 即可；没有字体时地图仍可生成，只是中文标签可能缺失。配置向导会自动发现常见字体并询问是否写入，不会未经确认安装系统软件。

### 小红书 MCP

小红书默认关闭。启用前准备一个本机 `xiaohongshu-mcp` StreamableHTTP 服务，把端点和可选的 `XHS_MCP_TOKEN` 写入本地配置，然后使用：

```sh
./trip xhs login
./trip xhs check
./trip xhs on
./trip xhs status
```

`./trip xhs logout` 可清理登录态，`./trip xhs off` 可关闭运行时开关。修改开关后重启 cheaptrip 才会重建工具列表。服务未运行或未登录时，Agent 会报告不可用并退回 Web 搜索；不会阻塞主流程。

小红书工具只读搜索笔记和评论。搜索结果限制数量并带随机间隔；不接入发布、评论、关注等操作。账号 cookies 保存在服务端数据目录，不进入 Web 媒体目录或 Git。

### 携程酒店评论

酒店评论爬虫是选装功能，需要 Node.js、Playwright 和人工扫码登录：

```sh
./trip hotel install
./trip hotel login
./trip hotel on
./trip hotel status
./trip hotel crawl <酒店 ID 或 URL>
```

不用时可运行 `./trip hotel off`，登出使用 `./trip hotel logout`。酒店工具只核验已经进入候选名单的少量酒店：先筛选、再爬取，每次规划约 2–3 家、每家最多 5 张图片。登录态含 cookies，保存在被忽略的本机文件中，不能提交。

### 固定域名 Tunnel

固定域名只适合已有 Cloudflare Named Tunnel 的部署。将域名和本机 Tunnel 配置文件路径放在 `.env`，用 `./trip setup check` 或 `./trip web domain` 做只读校验；本项目不会替你创建、登录或修改 Cloudflare 资源。

## 配置、数据与安全

### 配置文件

- `.env`：API key、Web 口令、字体路径和本机 Tunnel 信息；被 Git 忽略，真实值只在服务端读取。
- `config.toml`：从 `config.example.toml` 复制的本机运行配置；包括模型、Provider、思考强度、费用、会话、Web、认证和选装开关。
- `config.example.toml`：可公开的安全默认模板，XHS 默认关闭，不包含私人值。

LLM 配置可选择自动识别、GLM 或通用 OpenAI-compatible Provider；GLM 支持 `low`、`high`、`max` 思考强度。中转站是否真正执行某个强度取决于上游模型和协议，通用适配器不会伪造 GLM 私有字段。

### 本地运行数据

| 路径 | 内容 | 安全边界 |
|---|---|---|
| `sessions/` | 会话 JSON 与记忆笔记 | 含个人行程，已忽略 |
| `maps/` | 总览图、城市图和聚焦图 | 按会话保存，删除会话时联动清理 |
| `hotels/` | 酒店评论和图片 | 可能含个人旅行偏好，已忽略 |
| `data/` | 启用账号认证后的 SQLite 数据库 | 含账号和会话归属，已忽略 |
| `.toggles.json` | 本机选装工具开关 | 只在本机生效，已忽略 |
| `web/node_modules/`、`web/dist/`、`target/` | 依赖和构建产物 | 可重建，不提交 |

不要提交 `.env`、`config.toml`、账号数据库、cookies、会话、地图、酒店输出、二维码或任何 API key。公网运行必须设置口令、启用 HTTPS 和严格认证；不要把 token 放进截图、聊天记录或公开 issue。

## 架构与工具

数据流是“用户输入 → Core → Agent → LLM/工具 → AgentEvent → TUI 或 Web”。Core 与前端解耦，Agent 负责阶段提示词、工具循环、取消和事件，Session 负责历史与记忆，Web 层提供 REST、WebSocket、静态页面和受保护的媒体下发。

主要目录：

```text
src/agent.rs       Agent 循环、阶段提示、模式与工具白名单
src/core.rs        前端无关的 Core 与事件流
src/session.rs     会话、标题、记忆、用量与产物清理
src/llm.rs         OpenAI-compatible/GLM 流式客户端
src/tools/         搜索、天气、地图、车次、酒店和 MCP 工具
src/web/           REST、WebSocket、认证、设置和媒体服务
web/               React + TypeScript + Vite 前端
skills/            常驻总则、阶段指令和工具策略
```

当前主要工具：

- `search_web`：Exa 联网搜索；`search_xhs`、`read_xhs_note`：可选的小红书只读搜索。
- `get_weather`、`geocode`、`route_check`：天气、地理编码和高德驾车查证。
- `search_trains`：免登录直查 12306；明确车次、火车、高铁、动车、余票或时刻请求会优先走它。
- `cluster_pois`：纯几何 part 聚类，不发网络请求。
- `generate_overview_map`、`generate_city_map`：总览图与城市路线图。
- `search_hotel_reviews`：可选的携程评论与住客图片查证。
- `update_notes`：维护当前会话记忆；`update_knowledge` 目前只是未启用的知识库接口预留。

地图和高德请求有共享限流；路线、图片和标签布局由代码确定，模型只提供规划数据。Web 会校验会话路径后再下发地图和酒店图片，避免媒体路径穿越。

## 验证与已知限制

完整离线检查：

```sh
./trip verify
```

它会依次执行 Rust 格式检查、编译、Clippy、测试，以及 Web 测试、TypeScript 类型检查和生产构建。联网工具另有被忽略的实跑测试，是否执行取决于本机 API key、登录态和外部服务状态。

已知限制：

- 运行向导和启动器面向 Linux/WSL；Windows 用户应从 WSL 进入项目后运行 `./trip`。
- `./trip setup check` 是严格体检，不会替你修复权限、安装依赖或写配置；失败时按提示运行交互 setup。
- 网络搜索、天气、地图、12306、GLM、中转站、小红书和携程都受外部服务可用性、限流或登录态影响。
- 流式过程中断线重连只能恢复上一轮已落盘的历史；当前轮未结束的增量不会伪造为已保存消息。
- Web 的全局并发默认限制为 2；更多用户需要调整配置、代理和持久化方案。
- 知识库写入、跨站自动编排和携程远程二维码/短信网页登录没有启用；携程登录仍是服务端人工扫码。
- TUI 不内嵌显示图片，会返回 `maps/` 或 `hotels/` 路径；Web 会自动展示可用产物。

## 许可证与贡献

本项目以 GNU Affero 通用公共许可证第 3 版（AGPL-3.0-only）授权，完整条款见 [LICENSE](LICENSE)。你可以使用、修改、再发布本项目，也可以用于商业活动，但须遵守 AGPL-3.0 的要求。

如果你修改了本项目，并让用户通过网络与该修改版交互，AGPL-3.0 第 13 节要求向这些用户提供获得该版本对应源代码的途径。源码提供给该服务的用户即可；这不等于必须把修改提交给本仓库维护者，也不自动产生许可费。

如需闭源/专有商业使用，或希望免除 AGPL 的源码提供义务，请通过本仓库联系维护者洽谈单独的商业授权；费用与授权范围另行约定。AGPL 本身不会自动把商业收入支付给维护者。

提交改动前，至少运行 `./trip verify` 和 `git diff --check`，并确认没有把本机配置、密钥、会话、登录态或构建产物加入提交。
