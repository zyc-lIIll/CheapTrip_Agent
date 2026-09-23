#!/usr/bin/env bash
# cheaptrip 安全配置向导。
# 用法：./trip setup；只读体检：./trip setup check
set -Eeuo pipefail

project_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_dir"

usage() {
  cat <<'EOF'
用法：
  ./trip setup          交互式配置、安装和构建
  ./trip setup check    只读体检，不修改文件、依赖、容器或账号
EOF
}

if [[ "$#" -gt 1 || ( "$#" -eq 1 && "${1:-}" != "--check" ) ]]; then
  usage >&2
  exit 2
fi

CHECK=0
[[ "${1:-}" == "--check" ]] && CHECK=1
failures=0
warnings=0

ok() { printf '  ✅ %s\n' "$*"; }
warn() { warnings=$((warnings + 1)); printf '  ⚠ %s\n' "$*"; }
required_failure() { failures=$((failures + 1)); printf '  ❌ %s\n' "$*"; }
step_failure() { failures=$((failures + 1)); printf '  ❌ %s\n' "$*"; }

ask_yes_no() {
  local prompt="$1" default="${2:-n}" answer
  [[ "$CHECK" -eq 0 ]] || return 1
  if ! IFS= read -r -p "$prompt [$default/n]: " answer </dev/tty; then return 1; fi
  answer="${answer:-$default}"
  [[ "${answer,,}" != "n" ]]
}

env_value() {
  local key="$1"
  [[ -f .env && ! -L .env ]] || return 0
  awk -v key="$key" 'index($0, key "=") == 1 { print substr($0, length(key) + 2); exit }' .env | tr -d '\r'
}

single_line_value() {
  local value="$1"
  [[ "$value" != *$'\n'* && "$value" != *$'\r'* ]]
}

rewrite_env_file() {
  local key="$1" value="$2" line replacement line_terminated
  local found=0 wrote_any=0 output_terminated=1
  while :; do
    if IFS= read -r line; then
      line_terminated=1
    else
      line_terminated=0
      [[ -n "$line" ]] || break
    fi
    if [[ "$line" == "$key="* ]]; then
      if [[ "$found" -eq 1 ]]; then continue; fi
      replacement="${key}=${value}"
      line="$replacement"
      found=1
    fi
    if [[ "$line_terminated" -eq 1 ]]; then
      printf '%s\n' "$line"
      output_terminated=1
    else
      printf '%s' "$line"
      output_terminated=0
    fi
    wrote_any=1
  done < .env
  if [[ "$found" -eq 0 ]]; then
    if [[ "$wrote_any" -eq 1 && "$output_terminated" -eq 0 ]]; then printf '\n'; fi
    printf '%s=%s\n' "$key" "$value"
  fi
}

set_env_value() {
  local key="$1" value="$2" temp_file
  if ! single_line_value "$value"; then
    printf '  ❌ %s 不能包含换行或回车。\n' "$key" >&2
    return 1
  fi
  [[ -f .env && ! -L .env ]] || return 1
  temp_file="$(mktemp "$project_dir/.env.tmp.XXXXXX")" || return 1
  chmod 600 "$temp_file" 2>/dev/null || true
  if ! rewrite_env_file "$key" "$value" >"$temp_file"; then
    rm -f -- "$temp_file"
    return 1
  fi
  [[ -f .env && ! -L .env ]] || { rm -f -- "$temp_file"; return 1; }
  chmod 600 "$temp_file" 2>/dev/null || { rm -f -- "$temp_file"; return 1; }
  if ! mv -f -- "$temp_file" .env; then
    rm -f -- "$temp_file"
    return 1
  fi
  chmod 600 .env 2>/dev/null
}

read_secret() {
  local key="$1" label="$2" value
  if ! IFS= read -r -s -p "  $label：" value </dev/tty; then
    printf '\n'; step_failure "$key 输入失败"; return 1
  fi
  printf '\n'
  if ! single_line_value "$value"; then step_failure "$key 不能包含换行或回车"; return 1; fi
  if [[ -z "$value" ]]; then step_failure "$key 保持为空"; return 1; fi
  if set_env_value "$key" "$value"; then ok "$key 已安全写入 .env"; return 0; fi
  step_failure "$key 写入失败，原文件未被替换"
  return 1
}

ensure_local_file() {
  local file="$1" template="$2" label="$3"
  if [[ -L "$file" || ( -e "$file" && ! -f "$file" ) ]]; then
    required_failure "$label 不是普通文件，拒绝继续"; return 1
  fi
  if [[ -f "$file" ]]; then ok "$label 已存在，不覆盖"; return 0; fi
  if [[ "$CHECK" -eq 1 ]]; then required_failure "缺少 $label（只读体检不会创建）"; return 1; fi
  if [[ ! -f "$template" ]]; then step_failure "缺少模板 $template"; return 1; fi
  if cp -- "$template" "$file"; then ok "已从 $template 创建 $label"; return 0; fi
  step_failure "无法创建 $label"; return 1
}

check_command() {
  local command="$1" label="$2" hint
  if command -v "$command" >/dev/null 2>&1; then ok "$label 可用"; return 0; fi
  case "$command" in
    bash|curl) hint="可运行 sudo apt install $command" ;;
    npm) hint="请安装 Node.js 20 或更高版本（会一并提供 npm）" ;;
    *) hint="请按系统文档安装后重试" ;;
  esac
  required_failure "$label 缺失；$hint"
  return 1
}

check_platform() {
  local os_name
  os_name="$(uname -s 2>/dev/null || true)"
  if [[ "$os_name" == Linux* ]]; then ok "运行平台：Linux/WSL"; else required_failure "仅支持 Linux/WSL（当前：${os_name:-未知}）"; fi
  check_command bash "Bash" || true
}

check_tool_versions() {
  local node_major rust_toolchain
  if command -v cargo >/dev/null 2>&1; then
    ok "$(cargo --version)"
    if command -v rustup >/dev/null 2>&1; then
      rust_toolchain="$(rustup show active-toolchain 2>/dev/null || true)"
      if [[ "$rust_toolchain" == stable* ]]; then ok "Rust stable：已激活"; else required_failure "未检测到 Rust stable；请运行 rustup toolchain install stable"; fi
    else
      required_failure "无法确认 Rust stable；请安装 rustup 并激活 stable"
    fi
  else
    required_failure "Rust/cargo 缺失；请安装 rustup stable（不会由本向导静默安装）"
  fi
  if command -v node >/dev/null 2>&1; then
    node_major="$(node -p 'process.versions.node.split(".")[0]' 2>/dev/null || true)"
    if [[ "$node_major" =~ ^[0-9]+$ && "$node_major" -ge 20 ]]; then ok "Node.js $(node --version)"; else required_failure "Node.js 版本低于 20；请升级后重试"; fi
  else
    required_failure "Node.js 缺失；请安装 Node.js 20 或更高版本"
  fi
  check_command npm "npm" || true
  check_command curl "curl" || true
}

check_templates() {
  [[ -f .env.example ]] && ok ".env.example 存在" || required_failure "缺少 .env.example"
  [[ -f config.example.toml ]] && ok "config.example.toml 存在" || required_failure "缺少 config.example.toml"
  [[ -f web/package-lock.json ]] && ok "web/package-lock.json 存在" || required_failure "缺少 web/package-lock.json"
}

check_env_state() {
  if [[ -L config.toml || ( -e config.toml && ! -f config.toml ) ]]; then required_failure "config.toml 不是普通文件，拒绝读取";
  elif [[ ! -f config.toml ]]; then required_failure "缺少 config.toml；运行 ./trip setup 创建（只读体检不会创建）";
  else ok "config.toml 是普通文件"; fi
  if [[ -L .env || ( -e .env && ! -f .env ) ]]; then required_failure ".env 不是普通文件，拒绝读取"; return; fi
  if [[ ! -f .env ]]; then required_failure "缺少 .env；运行 ./trip setup 创建（只读体检不会创建）"; return; fi
  local mode key
  mode="$(stat -c '%a' .env 2>/dev/null || true)"
  if [[ "$mode" == 600 ]]; then ok ".env 权限为 600"; else required_failure ".env 权限为 ${mode:-未知}，应为 600（只读体检不会修改）"; fi
  for key in API_KEY AMAP_API_KEY EXA_API_KEY; do
    if [[ -n "$(env_value "$key")" ]]; then ok "$key 已配置（值不显示）"; else required_failure "$key 未配置（值不显示）"; fi
  done
  if [[ -n "$(env_value WEB_TOKEN)" ]]; then ok "WEB_TOKEN 已配置（值不显示）"; else warn "WEB_TOKEN 未配置；本机模式可用，公网模式需先设置"; fi
}

check_optional_state() {
  local font_path tunnel_config
  font_path="$(env_value MAP_FONT_PATH)"
  if [[ -n "$font_path" && -f "$font_path" ]]; then ok "CJK 字体已配置"; else warn "CJK 字体未配置或文件不存在（地图中文标签可能缺失）"; fi
  if command -v cloudflared >/dev/null 2>&1; then
    ok "cloudflared 可用（选装）"
    tunnel_config="$(env_value TRIP_TUNNEL_CONFIG)"
    if [[ -n "$tunnel_config" && -f "$tunnel_config" ]]; then
      if cloudflared tunnel --config "$tunnel_config" ingress validate >/dev/null 2>&1; then ok "Named Tunnel 配置有效（域名值不显示）"; else warn "Named Tunnel 配置校验失败（选装）"; fi
    else warn "未配置固定域名 Named Tunnel（选装）"; fi
  else warn "cloudflared 未安装（随机公网网址/固定域名为选装）"; fi
  if [[ -d scripts/hotel_crawl/node_modules ]]; then ok "酒店 Playwright 依赖已安装（选装）"; else warn "酒店 Playwright 依赖未安装（选装）"; fi
  if [[ -d "$HOME/xhs-mcp" ]]; then ok "小红书本地数据目录存在（选装）"; else warn "小红书 MCP 未配置（选装）"; fi
}

check_web_state() {
  [[ -d web/node_modules ]] && ok "Web node_modules 已存在" || required_failure "Web 依赖未安装（运行 ./trip setup）"
  [[ -f web/dist/index.html ]] && ok "Web 构建产物已存在" || required_failure "Web 构建产物缺失（运行 ./trip setup）"
}

check_rust_readonly() {
  local temp_target
  command -v cargo >/dev/null 2>&1 || return
  temp_target="$(mktemp -d /tmp/cheaptrip-setup-check.XXXXXX)" || { required_failure "无法创建 /tmp Rust 检查目录"; return; }
  if CARGO_TARGET_DIR="$temp_target" cargo check --locked --offline >/dev/null 2>&1; then ok "Rust 可编译（临时 target，离线只读检查）"; else required_failure "Rust 离线编译检查失败（未联网下载依赖）"; fi
  rm -rf -- "$temp_target"
}

run_required_step() {
  local label="$1"; shift
  if "$@"; then ok "$label"; return 0; fi
  step_failure "$label 失败"; return 1
}

create_local_files() {
  config_ready=0
  env_ready=0
  umask 077
  if ensure_local_file config.toml config.example.toml "config.toml"; then config_ready=1; fi
  if ensure_local_file .env .env.example ".env"; then
    if chmod 600 .env 2>/dev/null; then env_ready=1; else step_failure ".env 权限设置失败"; fi
  fi
}

configure_required_secrets() {
  local key label current
  for key in API_KEY AMAP_API_KEY EXA_API_KEY; do
    current="$(env_value "$key")"
    if [[ -n "$current" ]]; then ok "$key 已配置（值不显示）"; continue; fi
    label="$key"
    case "$key" in
      API_KEY) label="LLM API_KEY（输入不回显）" ;;
      AMAP_API_KEY) label="高德 AMAP_API_KEY（输入不回显）" ;;
      EXA_API_KEY) label="Exa EXA_API_KEY（输入不回显）" ;;
    esac
    read_secret "$key" "$label" || true
  done
}

configure_web_token() {
  local token
  [[ -n "$(env_value WEB_TOKEN)" ]] && { ok "WEB_TOKEN 已配置（值不显示）"; return; }
  if ask_yes_no "  生成随机 WEB_TOKEN（只保存到 .env，不显示口令）？" y; then
    if ! command -v openssl >/dev/null 2>&1; then warn "缺少 openssl，无法自动生成；可稍后手动填写 WEB_TOKEN"; return; fi
    token="$(openssl rand -hex 32 2>/dev/null || true)"
    if [[ "$token" =~ ^[0-9a-f]{64}$ ]] && set_env_value WEB_TOKEN "$token"; then ok "WEB_TOKEN 已生成并保存（值不显示）"; else warn "WEB_TOKEN 生成或写入失败（选装；公网模式前请手动配置）"; fi
  else warn "WEB_TOKEN 保持为空；本机模式可用，公网模式需要它"; fi
}

configure_font() {
  local current candidate
  current="$(env_value MAP_FONT_PATH)"
  [[ -n "$current" && -f "$current" ]] && { ok "CJK 字体已配置"; return; }
  warn "CJK 字体未配置（选装；不影响核心 setup）"
  if command -v fc-list >/dev/null 2>&1; then
    candidate="$(fc-list :lang=zh file 2>/dev/null | grep -iE 'noto|cjk|wqy|droid' | head -1 | sed 's/:$//' || true)"
    if [[ -n "$candidate" ]] && ask_yes_no "  发现 CJK 字体，写入 MAP_FONT_PATH？" y; then
      if set_env_value MAP_FONT_PATH "$candidate"; then ok "CJK 字体路径已保存"; else warn "字体路径写入失败（选装）"; fi
    fi
  fi
}

configure_tunnel_check() {
  local tunnel_config
  tunnel_config="$(env_value TRIP_TUNNEL_CONFIG)"
  [[ -z "$tunnel_config" ]] && { warn "固定域名 Named Tunnel 未配置（选装）"; return; }
  if command -v cloudflared >/dev/null 2>&1 && [[ -f "$tunnel_config" ]]; then
    if cloudflared tunnel --config "$tunnel_config" ingress validate >/dev/null 2>&1; then ok "固定域名 Named Tunnel 配置有效（域名值不显示）"; else warn "固定域名 Named Tunnel 配置校验失败（选装）"; fi
  else warn "固定域名配置不完整或未安装 cloudflared（选装）"; fi
}

configure_hotel() {
  if ! ask_yes_no "  安装携程酒店 Playwright（选装）？" n; then warn "跳过酒店 Playwright；之后可运行 ./trip hotel install"; return; fi
  if ./trip hotel install; then ok "酒店 Playwright 已安装"; else warn "酒店 Playwright 安装失败（选装）"; fi
}

set_xhs_enabled() {
  local temp_file
  temp_file="$(mktemp "$project_dir/config.toml.tmp.XXXXXX")" || return 1
  if ! awk 'BEGIN { in_xhs = 0; changed = 0 } /^\[/ { in_xhs = ($0 == "[xhs]") } in_xhs && /^enabled[[:space:]]*=/ && !changed { print "enabled = true"; changed = 1; next } { print } END { if (!changed) exit 2 }' config.toml >"$temp_file"; then
    rm -f -- "$temp_file"; return 1
  fi
  if ! mv -f -- "$temp_file" config.toml; then rm -f -- "$temp_file"; return 1; fi
}

configure_xhs() {
  local xhs_token="" docker_running=0
  if ! ask_yes_no "  配置小红书 MCP（选装）？" n; then warn "跳过小红书；之后可运行 ./trip setup"; return; fi
  if ! command -v docker >/dev/null 2>&1; then warn "未安装 Docker，跳过小红书（选装）"; return; fi
  if docker ps --format '{{.Names}}' 2>/dev/null | grep -qx xhs-mcp; then
    docker_running=1
    ok "容器 xhs-mcp 已运行"
  else
    if ! IFS= read -r -s -p "  新建容器的 AUTH_TOKEN（回车=不启用，不回显）：" xhs_token </dev/tty; then printf '\n'; warn "未读取到口令，跳过小红书容器"; return; fi
    printf '\n'
    if ! single_line_value "$xhs_token"; then warn "AUTH_TOKEN 不能包含换行，跳过小红书容器"; return; fi
    mkdir -p "$HOME/xhs-mcp/data" "$HOME/xhs-mcp/images"
    local env_args=(-e COOKIES_PATH=/app/data/cookies.json -e HOME=/app/data/home -e XDG_CONFIG_HOME=/app/data/config)
    [[ -n "$xhs_token" ]] && env_args+=(-e AUTH_TOKEN="$xhs_token")
    docker rm -f xhs-mcp >/dev/null 2>&1 || true
    if docker run -d --name xhs-mcp --restart unless-stopped -p 18060:18060 "${env_args[@]}" \
      -v "$HOME/xhs-mcp/data:/app/data" -v "$HOME/xhs-mcp/images:/app/images" xpzouying/xiaohongshu-mcp >/dev/null; then
      ok "容器 xhs-mcp 已启动"
      docker_running=1
      if [[ -n "$xhs_token" ]]; then
        if set_env_value XHS_MCP_TOKEN "$xhs_token"; then ok "XHS_MCP_TOKEN 已安全保存（值不显示）"; else warn "XHS_MCP_TOKEN 写入失败（选装）"; fi
      fi
    else warn "小红书容器启动失败（选装）"; return; fi
  fi
  if [[ "$docker_running" -eq 1 ]]; then
    if set_xhs_enabled; then ok "config.toml [xhs].enabled 已开启"; else warn "无法开启 config.toml [xhs]（选装）"; fi
    if ./trip xhs check; then ok "小红书服务连通"; elif ask_yes_no "  小红书尚未登录，现在打开登录流程？" n; then
      if ./trip xhs login; then ok "小红书登录流程完成"; else warn "小红书登录未完成（选装）"; fi
    else warn "跳过小红书登录（选装）"; fi
  fi
}

run_setup() {
  printf '═══════ cheaptrip 配置向导 ═══════\n'
  printf '支持范围：Linux/WSL；密钥输入不回显，公网口令只写入本机 .env。\n'
  check_platform; check_tool_versions; check_templates; create_local_files
  if [[ "$config_ready" -eq 1 && "$env_ready" -eq 1 ]]; then
    configure_required_secrets; configure_web_token; configure_font; configure_tunnel_check
  else
    warn "config.toml 或 .env 未就绪，跳过配置相关步骤"
  fi
  printf '── Web 依赖与构建 ──\n'
  if command -v npm >/dev/null 2>&1 && [[ -f web/package-lock.json ]]; then
    if run_required_step "Web 依赖已按 lockfile 安装" npm --prefix web ci; then run_required_step "Web 前端构建完成" npm --prefix web run build || true; fi
  else step_failure "无法执行 Web 依赖安装（需要 npm 和 web/package-lock.json）"; fi
  printf '── Rust 构建 ──\n'
  if command -v cargo >/dev/null 2>&1; then run_required_step "Rust 构建完成" cargo build --locked || true; else step_failure "无法执行 Rust 构建（缺少 cargo）"; fi
  printf '── 选装集成 ──\n'
  configure_hotel
  if [[ "$config_ready" -eq 1 && "$env_ready" -eq 1 ]]; then configure_xhs; else warn "配置文件未就绪，跳过小红书（选装）"; fi
  if [[ "$failures" -gt 0 ]]; then printf '═══════ 配置未完成：%d 个必需步骤失败，%d 个选装警告 ═══════\n' "$failures" "$warnings"; return 1; fi
  printf '═══════ 配置完成（%d 个选装警告）═══════\n' "$warnings"
  printf '下一步：./trip web local | ./trip web random | ./trip web domain\n'
}

run_check() {
  printf '═══════ cheaptrip 只读体检 ═══════\n'
  printf '本模式不会创建/修改配置、依赖、构建产物、容器或账号。\n'
  check_platform; check_tool_versions; check_templates; check_env_state; check_web_state; check_rust_readonly; check_optional_state
  if [[ "$failures" -gt 0 ]]; then printf '═══════ 体检失败：%d 个必需项失败，%d 个选装警告 ═══════\n' "$failures" "$warnings"; return 1; fi
  printf '═══════ 体检通过（%d 个选装警告）═══════\n' "$warnings"
}

if [[ "$CHECK" -eq 1 ]]; then run_check; else run_setup; fi
