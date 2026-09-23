#!/usr/bin/env bash
# 在 WSL/Linux 中启动拾光者 Web；公网模式额外连接 Cloudflare Tunnel。
# 用法：run-trip.sh [local|random|domain]；默认 random 以兼容旧入口。
set -Eeuo pipefail

project_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_dir"

mode="${1:-random}"
case "$mode" in
  local | random | domain) ;;
  *)
    printf '用法：%s [local|random|domain]\n' "$0" >&2
    exit 2
    ;;
esac

# 从 Windows 调入 WSL 时 bash 不是登录 shell，默认不会读取 ~/.profile；
# Rustup 的 cargo 通常在 ~/.cargo/env 中加入 PATH，因此在此显式加载。
if [[ -f "$HOME/.cargo/env" ]]; then
  # shellcheck disable=SC1090
  source "$HOME/.cargo/env"
fi

required_commands=(npm cargo curl)
if [[ "$mode" != local ]]; then
  required_commands+=(cloudflared)
fi
for command in "${required_commands[@]}"; do
  if ! command -v "$command" >/dev/null 2>&1; then
    printf '缺少命令：%s\n' "$command" >&2
    exit 1
  fi
done

# 仅读取指定键，不打印值，也不把 .env 当作 shell 脚本执行。
read_env_value() {
  local key="$1"
  sed -n "s/^${key}=//p" .env | head -n 1 | tr -d '\r'
}

if [[ "$mode" != local ]]; then
  if [[ ! -f .env ]]; then
    printf '缺少 .env；请从 .env.example 创建并设置 WEB_TOKEN。\n' >&2
    exit 1
  fi

  web_token="$(read_env_value WEB_TOKEN)"
  if [[ -z "$web_token" ]]; then
    printf 'WEB_TOKEN 为空；为避免公网接口无鉴权，拒绝启动 Tunnel。\n' >&2
    exit 1
  fi
fi

if [[ "$mode" == domain ]]; then
  trip_domain="$(read_env_value TRIP_DOMAIN)"
  tunnel_config="$(read_env_value TRIP_TUNNEL_CONFIG)"

  if [[ -z "$trip_domain" || -z "$tunnel_config" ]]; then
    printf '固定域名模式需要在 .env 设置 TRIP_DOMAIN 和 TRIP_TUNNEL_CONFIG。\n' >&2
    exit 1
  fi
  if [[ ! "$trip_domain" =~ ^[A-Za-z0-9.-]+$ ]]; then
    printf 'TRIP_DOMAIN 不是有效的主机名。\n' >&2
    exit 1
  fi
  if [[ ! -f "$tunnel_config" ]]; then
    printf '找不到 Named Tunnel 配置：%s\n' "$tunnel_config" >&2
    exit 1
  fi

  cloudflared tunnel --config "$tunnel_config" ingress validate >/dev/null
  domain_pattern="${trip_domain//./\\.}"
  if ! grep -Eq "^[[:space:]]*-[[:space:]]*hostname:[[:space:]]*${domain_pattern}[[:space:]]*$" "$tunnel_config"; then
    printf 'Named Tunnel 配置中没有与 TRIP_DOMAIN 匹配的 hostname。\n' >&2
    exit 1
  fi
fi

cleanup() {
  if [[ -n "${tunnel_pid:-}" ]] && kill -0 "$tunnel_pid" 2>/dev/null; then
    kill "$tunnel_pid" 2>/dev/null || true
    wait "$tunnel_pid" 2>/dev/null || true
  fi
  if [[ -n "${web_pid:-}" ]] && kill -0 "$web_pid" 2>/dev/null; then
    kill "$web_pid" 2>/dev/null || true
    wait "$web_pid" 2>/dev/null || true
  fi
  if [[ -n "${tunnel_log:-}" ]]; then
    rm -f "$tunnel_log"
  fi
}
trap cleanup EXIT INT TERM

printf '构建正式前端…\n'
(
  cd web
  npm run build
)

if curl -sS --max-time 1 http://127.0.0.1:8080/ >/dev/null 2>&1; then
  printf '端口 8080 已有 Web 服务运行；为避免连接旧进程，已停止启动。请先关闭旧服务。\n' >&2
  exit 1
fi

printf '启动 Rust Web 服务…\n'
cargo run -- web >"/tmp/cheaptrip-web.log" 2>&1 &
web_pid=$!

ready=false
for _ in $(seq 1 30); do
  if [[ "$mode" == local ]]; then
    if curl -sS --max-time 1 http://127.0.0.1:8080/ >/dev/null; then
      ready=true
      break
    fi
  elif curl -fsS --max-time 1 http://127.0.0.1:8080/ >/dev/null; then
    ready=true
    break
  fi
  if ! kill -0 "$web_pid" 2>/dev/null; then
    break
  fi
  sleep 1
done

if [[ "$ready" == true ]]; then
  sleep 1
  if ! kill -0 "$web_pid" 2>/dev/null; then
    ready=false
  fi
fi

if [[ "$ready" != true ]]; then
  printf 'Rust Web 服务未能在 30 秒内启动；最近日志：\n' >&2
  tail -n 40 /tmp/cheaptrip-web.log >&2 || true
  exit 1
fi

if [[ "$mode" == local ]]; then
  printf '\n本机 Web 已就绪：http://127.0.0.1:8080\n'
  printf '按 Ctrl+C 会停止 Rust Web 服务。\n\n'
  wait "$web_pid"
  exit "$?"
fi

tunnel_log="$(mktemp /tmp/cheaptrip-tunnel.XXXXXX.log)"

if [[ "$mode" == random ]]; then
  printf '本机服务已就绪，正在申请临时 Demo 网址…\n'
  # 显式忽略用户目录中的 Named Tunnel 配置，确保此模式始终创建随机 Quick Tunnel。
  cloudflared tunnel --config /dev/null --url http://127.0.0.1:8080 --logfile "$tunnel_log" > /dev/null 2>&1 &
  tunnel_pid=$!

  demo_url=""
  for _ in $(seq 1 30); do
    demo_url="$(sed -nE 's|.*(https://[-a-z0-9]+\.trycloudflare\.com).*|\1|p' "$tunnel_log" | head -n 1)"
    if [[ -n "$demo_url" ]]; then
      break
    fi
    if ! kill -0 "$tunnel_pid" 2>/dev/null; then
      break
    fi
    sleep 1
  done

  if [[ -z "$demo_url" ]]; then
    printf '未能在 30 秒内取得临时 Demo 网址；最近日志：\n' >&2
    tail -n 40 "$tunnel_log" >&2 || true
    exit 1
  fi

  printf '\n临时 Demo 网址：%s\n' "$demo_url"
  printf '访问时请在网址后追加 ?token=你的WEB_TOKEN；脚本不会打印口令。\n'
  printf '该网址每次启动都会变化；按 Ctrl+C 会同时停止 Tunnel 与 Rust 服务。\n\n'
else
  printf '本机服务已就绪，正在连接固定域名 Tunnel…\n'
  cloudflared tunnel --config "$tunnel_config" --logfile "$tunnel_log" run > /dev/null 2>&1 &
  tunnel_pid=$!

  domain_ready=false
  for _ in $(seq 1 30); do
    if curl -fsS --max-time 2 "https://${trip_domain}/" >/dev/null 2>&1; then
      domain_ready=true
      break
    fi
    if ! kill -0 "$tunnel_pid" 2>/dev/null; then
      break
    fi
    sleep 1
  done

  if [[ "$domain_ready" != true ]]; then
    printf '固定域名 Tunnel 未能在 30 秒内就绪；最近日志：\n' >&2
    tail -n 40 "$tunnel_log" >&2 || true
    exit 1
  fi

  printf '\n固定网址：https://%s\n' "$trip_domain"
  printf '访问时请在网址后追加 ?token=你的WEB_TOKEN；脚本不会打印口令。\n'
  printf '该网址保持不变；按 Ctrl+C 会同时停止 Tunnel 与 Rust 服务。\n\n'
fi

wait "$tunnel_pid"
