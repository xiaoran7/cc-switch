#!/usr/bin/env bash
# 从远端机器验证宿主机 cc-switch 代理的 HTTPS 可达性。
#
# 用法（在远端节点上跑）：
#   ./verify-tailnet-proxy.sh <host> <port> <api-key>
#
# 检查项：
#   1. MagicDNS 解析到 tailnet IP
#   2. TCP 端口可达
#   3. TLS 握手 + 证书链校验（必须是公共 CA 签发，不能 -k 跳过）
#   4. 证书 SAN 包含目标主机名
#   5. POST /v1/messages 拿到真实推理响应
set -uo pipefail

HOST="${1:?用法: $0 <host> <port> <api-key>}"
PORT="${2:?缺少端口}"
KEY="${3:?缺少 API Key}"

fail=0
ok()   { printf '  \033[32m✓\033[0m %s\n' "$1"; }
bad()  { printf '  \033[31m✗\033[0m %s\n' "$1"; fail=1; }
info() { printf '  · %s\n' "$1"; }

echo "=== 1. DNS 解析 ==="
RESOLVED="$(getent hosts "$HOST" 2>/dev/null | awk '{print $1}' | head -1)"
if [ -n "$RESOLVED" ]; then
  ok "$HOST -> $RESOLVED"
  case "$RESOLVED" in
    100.*) info "落在 CGNAT / tailnet 段" ;;
    *)     info "警告：不在 100.64.0.0/10 段" ;;
  esac
else
  bad "解析失败"
fi

echo
echo "=== 2. TCP 可达 ==="
if timeout 8 bash -c "</dev/tcp/$HOST/$PORT" 2>/dev/null; then
  ok "$HOST:$PORT 可连"
else
  bad "$HOST:$PORT 连不上"
  echo
  echo "结论：TCP 层就不通，后面的检查没有意义。"
  exit 1
fi

echo
echo "=== 3. TLS 握手与证书链 ==="
CERT_OUT="$(echo | timeout 12 openssl s_client -connect "$HOST:$PORT" -servername "$HOST" 2>&1)"
VERIFY="$(printf '%s' "$CERT_OUT" | grep -oE 'Verify return code: [0-9]+ \([^)]*\)' | head -1)"
if printf '%s' "$CERT_OUT" | grep -q "Verify return code: 0 (ok)"; then
  ok "证书链校验通过（公共 CA，无需 -k）"
elif [ -n "$VERIFY" ]; then
  bad "证书链校验失败：$VERIFY"
else
  bad "TLS 握手失败（可能代理没开 TLS，或端口不是 HTTPS）"
fi

echo
echo "=== 4. 证书 SAN ==="
SAN="$(printf '%s' "$CERT_OUT" | openssl x509 -noout -ext subjectAltName 2>/dev/null | tr -d ' ' | tr ',' '\n' | grep -i "DNS:$HOST" || true)"
if [ -n "$SAN" ]; then
  ok "SAN 包含 $HOST"
else
  bad "SAN 不含 $HOST"
  printf '%s' "$CERT_OUT" | openssl x509 -noout -ext subjectAltName 2>/dev/null | sed 's/^/    /'
fi

echo
echo "=== 5. 真实推理请求 ==="
BODY='{"model":"claude-haiku-4-5","max_tokens":16,"messages":[{"role":"user","content":"reply with the single word: pong"}]}'
RESP="$(curl -sS --max-time 60 -w '\n__STATUS__%{http_code}' \
  -X POST "https://$HOST:$PORT/v1/messages" \
  -H "content-type: application/json" \
  -H "anthropic-version: 2023-06-01" \
  -H "x-api-key: $KEY" \
  -H "authorization: Bearer $KEY" \
  -d "$BODY" 2>&1)"

STATUS="$(printf '%s' "$RESP" | sed -n 's/.*__STATUS__\([0-9]*\)$/\1/p')"
PAYLOAD="$(printf '%s' "$RESP" | sed 's/__STATUS__[0-9]*$//')"

if [ "$STATUS" = "200" ]; then
  ok "HTTP 200"
  MODEL="$(printf '%s' "$PAYLOAD" | grep -oE '"model":"[^"]*"' | head -1)"
  [ -n "$MODEL" ] && info "实际模型：$MODEL"
  TEXT="$(printf '%s' "$PAYLOAD" | grep -oE '"text":"[^"]*"' | head -1)"
  [ -n "$TEXT" ] && info "回复片段：$TEXT"
else
  bad "HTTP $STATUS"
  printf '%s' "$PAYLOAD" | head -c 600 | sed 's/^/    /'
  echo
fi

echo
if [ "$fail" -eq 0 ]; then
  printf '\033[32m结论：全部通过。\033[0m\n'
else
  printf '\033[31m结论：有检查项失败。\033[0m\n'
fi
exit "$fail"
