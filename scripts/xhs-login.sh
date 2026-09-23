#!/usr/bin/env bash
# 小红书 xiaohongshu-mcp 登录辅助：生成登录二维码 / 查询登录状态 / 退出账号 / 管理服务门禁。
# 二维码保存到「当前工作目录」——在哪个项目下运行就落在哪个项目文件夹下。
# 用法：
#   ./trip xhs login               # 生成 ./xhs_login_qr.png，扫码后按回车自动复查登录态
#   ./trip xhs check               # 只查询登录状态（最快的连通性检查）
#   ./trip xhs logout              # 退出小红书账号（删 cookies，之后需重新扫码）
#   ./trip xhs token 新口令         # 启用/更换服务门禁 AUTH_TOKEN（重建容器+同步 .env；登录态保留）
#   ./trip xhs token off            # 关闭门禁（重建容器不带 AUTH_TOKEN，.env 移除 XHS_MCP_TOKEN）
# 环境变量：
#   XHS_MCP_URL    默认 http://localhost:18060/mcp
#   XHS_MCP_TOKEN  服务开启 AUTH_TOKEN 鉴权时必填；当前目录有 .env 时自动读取
set -euo pipefail

URL="${XHS_MCP_URL:-http://localhost:18060/mcp}"
if [ -z "${XHS_MCP_TOKEN:-}" ] && [ -f .env ]; then
  # .env 里没有该行时 grep 退出码为 1，|| true 防 set -e 静默退出
  XHS_MCP_TOKEN="$(grep -E '^XHS_MCP_TOKEN=' .env | head -1 | cut -d= -f2- | tr -d '\r' || true)"
fi
AUTH=()
[ -n "${XHS_MCP_TOKEN:-}" ] && AUTH=(-H "Authorization: Bearer $XHS_MCP_TOKEN")

post() { curl -s --max-time 90 "$URL" -H "Content-Type: application/json" \
  -H "Accept: application/json, text/event-stream" "${AUTH[@]}" "$@"; }

# 1) initialize 握手，捕获 Mcp-Session-Id
SID=$(post -D - -o /dev/null -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"xhs-login","version":"1.0"}}}' \
  | tr -d '\r' | awk 'tolower($1)=="mcp-session-id:"{print $2}')
if [ -z "$SID" ]; then
  echo "❌ 无法连接 $URL —— 服务未启动？（docker ps 看 xhs-mcp 容器；WSL 里先确认 dockerd 在跑）"
  exit 1
fi
# 2) 握手完成通知
post -H "Mcp-Session-Id: $SID" -o /dev/null -d '{"jsonrpc":"2.0","method":"notifications/initialized"}'
# 3) 工具调用
call() { post -H "Mcp-Session-Id: $SID" \
  -d "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"$1\",\"arguments\":$2}}"; }

if [ "${1:-}" = "check" ]; then
  call check_login_status '{}' | python3 -c 'import sys,json; print(json.load(sys.stdin)["result"]["content"][0]["text"])'
  exit 0
fi

if [ "${1:-}" = "logout" ]; then
  call delete_cookies '{}' | python3 -c 'import sys,json; print(json.load(sys.stdin)["result"]["content"][0]["text"])'
  echo "（登录态已清除；下次使用前重新运行 ./trip xhs login 扫码）"
  exit 0
fi

# 门禁管理：重建容器（cookies 在 ~/xhs-mcp/data 数据卷里，登录态不受影响）+ 同步当前项目 .env
if [ "${1:-}" = "token" ]; then
  NEW="${2:-}"
  if [ -z "$NEW" ]; then
    read -rs -p "输入新门禁口令（输入不回显；留空取消，输 off 关闭门禁）: " NEW; echo
  fi
  [ -z "$NEW" ] && { echo "已取消"; exit 0; }
  command -v docker >/dev/null || { echo "❌ 未安装 docker"; exit 1; }
  docker rm -f xhs-mcp >/dev/null 2>&1 || true
  mkdir -p "$HOME/xhs-mcp/data" "$HOME/xhs-mcp/images"
  ENVARGS=(-e COOKIES_PATH=/app/data/cookies.json -e HOME=/app/data/home -e XDG_CONFIG_HOME=/app/data/config)
  if [ "$NEW" = "off" ]; then
    echo "→ 关闭门禁：重建容器（不带 AUTH_TOKEN）…"
  else
    ENVARGS+=(-e AUTH_TOKEN="$NEW")
    echo "→ 启用/更换门禁：重建容器（带 AUTH_TOKEN）…"
  fi
  docker run -d --name xhs-mcp --restart unless-stopped -p 18060:18060 "${ENVARGS[@]}" \
    -v "$HOME/xhs-mcp/data:/app/data" -v "$HOME/xhs-mcp/images:/app/images" \
    xpzouying/xiaohongshu-mcp >/dev/null
  # 同步当前项目 .env（grep -v 无匹配行时退出码为 1，|| true 防 set -e 退出）
  if [ "$NEW" = "off" ]; then
    if [ -f .env ]; then
      grep -v '^XHS_MCP_TOKEN=' .env > .env.tmp || true
      mv .env.tmp .env
    fi
    echo "✅ 门禁已关闭，.env 中的 XHS_MCP_TOKEN 已移除（config.toml 的 token_env 留着无碍）"
  else
    if [ -f .env ]; then
      grep -v '^XHS_MCP_TOKEN=' .env > .env.tmp || true
      mv .env.tmp .env
    fi
    echo "XHS_MCP_TOKEN=$NEW" >> .env
    echo "✅ 门禁已启用：容器已用新口令重建，当前项目 .env 的 XHS_MCP_TOKEN 已同步"
    echo "   小红书登录态不受影响；重启 cheaptrip 生效；口令位置记得登记 SECRETS.md"
  fi
  exit 0
fi

echo "获取登录二维码中…"
RESP="$(call get_login_qrcode '{}')"
RESP="$RESP" python3 - <<'PY'
import os, json, base64, sys
raw = os.environ.get("RESP", "")
try:
    d = json.loads(raw)
except Exception:
    sys.exit(f"❌ 响应解析失败: {raw[:300]}")
ok = False
for b in d.get("result", {}).get("content", []):
    if b.get("type") == "image" and b.get("data"):
        open("xhs_login_qr.png", "wb").write(base64.b64decode(b["data"]))
        print("✅ 二维码已保存到当前目录: xhs_login_qr.png")
        ok = True
        break
    if b.get("type") == "text":
        print(b.get("text", ""))
if not ok:
    sys.exit("❌ 响应中没有二维码图片")
PY

echo "→ 用小红书 App 扫描当前目录下的 xhs_login_qr.png，完成登录后按回车验证…"
read -r
call check_login_status '{}' | python3 -c 'import sys,json; print(json.load(sys.stdin)["result"]["content"][0]["text"])'
