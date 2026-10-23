#!/usr/bin/env bash
# 切换 cc-switch 代理的监听地址，并同步写进 Claude Desktop 的配置。
#
# 用法：
#   ./set-proxy-address.sh tailnet    # 监听 + 客户端都用 MagicDNS 名，走 HTTPS
#   ./set-proxy-address.sh loopback   # 回到 127.0.0.1，走明文 HTTP
#   ./set-proxy-address.sh status     # 只看当前状态
#
# 前提：tailnet 模式需要 <host>.crt/.key 已放在 ~/.cc-switch/tls/ 下
#       （用 `tailscale cert` 签发）。
set -euo pipefail

DB="$HOME/.cc-switch/cc-switch.db"
HOST="portable-pc.tail462ee4.ts.net"
PORT=15721

die() { printf '\033[31m错误: %s\033[0m\n' "$1" >&2; exit 1; }

[ -f "$DB" ] || die "找不到数据库 $DB"

# cc-switch 必须关掉再改库，否则它退出时会把内存里的旧值写回
if tasklist 2>/dev/null | grep -qi "cc-switch.exe"; then
  die "cc-switch 正在运行，先退出（托盘 → 退出），改完再启动"
fi

show_status() {
  echo "=== 数据库里的监听配置 ==="
  python - "$DB" <<'PY'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
for app, en, addr, port in c.execute(
    "SELECT app_type, proxy_enabled, listen_address, listen_port FROM proxy_config"
):
    print(f"  {app:12} enabled={en}  {addr}:{port}")
PY
  echo
  echo "=== 设置 ==="
  python - "$HOME/.cc-switch/settings.json" <<'PY'
import json, sys, pathlib
p = pathlib.Path(sys.argv[1])
s = json.loads(p.read_text(encoding="utf-8"))
print(f"  proxy_client_host = {s.get('proxy_client_host')!r}")
print(f"  proxy_tls_cert    = {s.get('proxy_tls_cert')!r}")
PY
  echo
  echo "=== 证书 ==="
  ls -1 "$HOME/.cc-switch/tls/" 2>/dev/null | sed 's/^/  /' || echo "  （无 tls 目录）"
  echo
  echo "=== Claude Desktop 当前网关地址 ==="
  python - "$LOCALAPPDATA/Claude-3p/configLibrary/00000000-0000-4000-8000-000000157210.json" <<'PY'
import json, sys, pathlib
p = pathlib.Path(sys.argv[1])
if p.exists():
    d = json.loads(p.read_text(encoding="utf-8"))
    print(f"  {d.get('inferenceGatewayBaseUrl')!r}")
else:
    print("  （找不到配置文件）")
PY
}

case "${1:-status}" in
  status)
    show_status
    ;;

  loopback)
    python - "$DB" <<'PY'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
c.execute("UPDATE proxy_config SET listen_address='127.0.0.1'")
c.commit()
print("监听地址已改回 127.0.0.1")
PY
    python - "$HOME/.cc-switch/settings.json" <<'PY'
import json, sys, pathlib
p = pathlib.Path(sys.argv[1])
s = json.loads(p.read_text(encoding="utf-8"))
s.pop("proxy_client_host", None)
s.pop("proxy_tls_cert", None)
p.write_text(json.dumps(s, ensure_ascii=False, indent=2), encoding="utf-8")
print("已清掉 proxy_client_host / proxy_tls_cert")
PY
    echo
    echo "下一步：启动 cc-switch，它会按 127.0.0.1 重写客户端配置（明文 HTTP）。"
    ;;

  tailnet)
    [ -f "$HOME/.cc-switch/tls/$HOST.crt" ] || die "缺证书 $HOST.crt，先跑 tailscale cert"
    [ -f "$HOME/.cc-switch/tls/$HOST.key" ] || die "缺私钥 $HOST.key"

    python - "$DB" "$HOST" <<'PY'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
c.execute("UPDATE proxy_config SET listen_address=?", (sys.argv[2],))
c.commit()
print(f"监听地址已改为 {sys.argv[2]}")
PY
    python - "$HOME/.cc-switch/settings.json" "$HOST" <<'PY'
import json, sys, pathlib
p = pathlib.Path(sys.argv[1])
s = json.loads(p.read_text(encoding="utf-8"))
s["proxy_client_host"] = sys.argv[2]
# 留空表示按 <数据目录>/tls/<host>.crt 找，这里显式写出来更直观
s.pop("proxy_tls_cert", None)
p.write_text(json.dumps(s, ensure_ascii=False, indent=2), encoding="utf-8")
print(f"proxy_client_host 已设为 {sys.argv[2]}")
PY
    echo
    echo "下一步：启动 cc-switch。它会监听 $HOST:$PORT 并做 TLS 终结，"
    echo "      同时把 Claude Desktop 的 inferenceGatewayBaseUrl 写成 https://$HOST:$PORT/... "
    ;;

  *)
    die "未知参数: $1（可用: tailnet | loopback | status）"
    ;;
esac
