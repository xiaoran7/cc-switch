# 本 Fork 说明（tailnet HTTPS 支持）

## 上游溯源

| 项 | 值 |
|---|---|
| 上游仓库 | [farion1231/cc-switch](https://github.com/farion1231/cc-switch) |
| 上游版本基线 | `v4.0.25` |
| 上游协议 | MIT |
| 上游作者 | Jason Young |
| 本 Fork 分支 | `feat/tailnet-https` |
| 本 Fork 版本号 | `4.0.25+tailnet.3` |

**主权归属上游。** 除下方「本 Fork 改动的模块」一节列出的内容外，其余全部代码、文档、设计均属上游。
本 Fork 不做无记录魔改；上游发布新版时按
`git fetch upstream --tags && git rebase --onto <新 tag> <上个基线 tag> feat/tailnet-https` 跟进。

## ⚠️ 自动更新已禁用（防止被官方版覆盖）

上游更新器会把安装目录里的二进制换成官方发布版，而上游**至今没有** TLS 终结和域名监听。
2026-11-02 就是这么丢的：官方 4.0.24 在运行中被装上，启动报
`invalid socket address syntax`，监听地址退回 `127.0.0.1`，Tailnet HTTPS 整条断掉。

因此 `src-tauri/tauri.conf.json` 的 `plugins.updater.endpoints` 已被**清空**：

```json
"updater": { "pubkey": "...", "endpoints": [] }
```

上游 updater 在端点为空时返回 `EmptyEndpoints`，不会下载任何安装包。前端
`checkForUpdate()` 把这个错误映射为 `disabled`，与"已是最新"同样静默处理。

**后果与义务**：本 Fork 不再自动跟进上游，改由自动化日常同步脚本维护：

```bash
python scripts/sync_upstream.py
```

该脚本会自动 fetch 上游、创建本地安全备份、执行 rebase、自动处理版本号与 CHANGELOG 冲突（通过 `scripts/resolve_conflicts.py`）、运行前端 typecheck 与 Rust 单元测试，并在成功后将最新分支推送到个人的 `fork/feat/tailnet-https` 远端。

需要临时恢复官方端点时：

```
https://dl.ccswitch.io/latest.json
https://github.com/farion1231/cc-switch/releases/latest/download/latest.json
```

## 本 Fork 改动的模块

改动集中在**代理监听地址的协议选择与 TLS 终结**，共 6 个文件：

| 文件 | 改动 |
|---|---|
| `src-tauri/src/services/proxy.rs` | `proxy_origin()` 按主机名选协议；新增 `is_loopback_host()` |
| `src-tauri/src/proxy/tls.rs` | **新增**。证书解析、rustls acceptor 构造、单元测试 |
| `src-tauri/src/proxy/server.rs` | accept 循环接入 TLS 终结；抽出 `peek_header_cases()` / `serve_conn()`；支持域名解析 |
| `src-tauri/src/proxy/mod.rs` | 注册 `tls` 模块 |
| `src-tauri/src/settings.rs` | 新增 `proxy_tls_cert` 设置项与读写函数 |
| `src-tauri/src/commands/proxy.rs` | 新增 `get_proxy_tls_cert` / `set_proxy_tls_cert` / `get_proxy_endpoints_info` 命令 |
| `src-tauri/src/lib.rs` | 注册上述三个命令 |
| `src-tauri/src/mode/controller.rs` | 更新一个断言旧行为的测试 |
| `src-tauri/Cargo.toml` | 新增 `rustls-pemfile = "2"` |
| `src/components/settings/sections/RoutingSection.tsx` | **前端增强**。监听地址预设下拉选择器、自定义输入支持与实时证书与协议状态条 |
| `src/types/proxy.ts` / `src/lib/api/proxy.ts` / `src/lib/query/proxy.ts` | 代理端点候选与 TLS 状态数据模型与 React Query 绑定 |

## 为什么需要这个改动

Claude Desktop 对 `inferenceGatewayBaseUrl` 的校验（从 `app.asar` 挖出的实现）：

```js
n === "https:" ? !e.rejectLoopback || !_Ee(r)
: n === "http:" && (!!e.allowHttp || !!e.allowLoopbackHttp && vs.has(r))
```

其中 `vs = new Set(["127.0.0.1", "localhost", "[::1]"])` 是**字面量白名单**。

即：**明文 `http` 只放行那三个字符串，其余主机一律必须 `https`。**

上游的 `proxy_origin()` 无条件拼 `http://`，所以一旦把监听/客户端地址设成非回环地址
（tailnet IP、私网 IP、域名），桌面端会在**配置校验阶段**就报：

```
Invalid custom3p managed config: inferenceGatewayBaseUrl
(set but invalid: .: must use https (or http on loopback))
failingField: inferenceGatewayBaseUrl
```

连请求都发不出去。这个改动补的就是这一层。

## 行为

| 监听/客户端地址 | 客户端拿到的地址 | 代理监听 |
|---|---|---|
| `127.0.0.1` / `localhost` / `::1` / `0.0.0.0` | `http://...` | 明文 HTTP（与上游一致） |
| 其它任意主机 | `https://...` | **TLS 终结** |

**回环路径完全不变**，不增加任何 TLS 开销。只有非回环地址才启用 TLS。

## 证书

证书来源按优先级：

1. 设置项 `proxy_tls_cert`（PEM 路径），私钥默认同目录同名、扩展名换成 `.key`
2. `<应用数据目录>/tls/<监听主机>.crt` 与 `.key`

Windows 的应用数据目录是 `%USERPROFILE%\.cc-switch`。

推荐用 Tailscale 签发真实证书（远端无需额外信任根，因为 `*.ts.net` 由公共 CA 签发）：

```bash
tailscale cert --cert-file "%USERPROFILE%\.cc-switch\tls\<host>.crt" \
               --key-file  "%USERPROFILE%\.cc-switch\tls\<host>.key" <host>
```

**找不到证书时不静默降级成明文**——那会让客户端拿到一个连不上的 `https` 地址。
代理启动会直接失败并给出明确提示。

## 验证

```bash
cd src-tauri
cargo test --lib proxy::tls          # 5 个单测（回环检测、URL协议推断、约定路径查找、真实TLS握手与传输、明文请求阻断）
cargo test --lib                     # 全量库测试
```

从远端 Linux 机器（如远端开发节点）全链路验证：
```bash
./scripts/verify-tailnet-proxy.sh <your-tailnet-host>.ts.net 15721 <claude_desktop_gateway_token>
```
验证包含 5 项硬性检查：
1. MagicDNS 解析
2. TCP 端口直连
3. TLS 握手及公共 CA 根证书链校验（无需任何 `-k`）
4. 证书 SAN 匹配
5. 真实 POST `/claude-desktop/v1/messages` 收到 Anthropic 模型响应

## 已知限制

- **只做 TLS 终结，不做客户端证书校验**：能连到监听端口的人都能访问代理，靠 Tailscale ACL 兜底。
- **证书不自动续期**：`tailscale cert` 签发的证书有有效期，过期需重新签发并重启代理。
  后续可以加一个后台任务自动续期（上游未实现）。
- **TLS 连接抓不到请求头原始大小写**：TLS 首包是 ClientHello，`peek` 不到 HTTP 头，
  所以 `preserve_header_case` 在 TLS 路径上退化为 hyper 默认行为。这会影响发往上游的
  header 大小写还原度，但对功能无影响（HTTP/1.1 头名大小写不敏感）。
- **远端机器不跑这个 Fork**：本 Fork 运行在宿主机上，远端机器仅作为**客户端**直连。

## 踩坑：rustls CryptoProvider 必须显式安装

这个 crate 的依赖树里，`hyper-rustls` 拉的是 `aws-lc-rs`、`reqwest` 拉的是 `ring`，
**两个 provider 同时启用**。rustls 0.23 无法自动判定，任何 `ServerConfig::builder()`
都会 panic：

```
Could not automatically determine the process-level CryptoProvider from Rustls crate features.
```

上游代码从没建过 `ServerConfig`，所以这个雷一直埋着，加 TLS 才会踩到。
修法是 `tls::install_crypto_provider()`（`Once` 保证只跑一次），在 `build_acceptor()` 入口调用。

**这个 bug 是单测抓出来的**，没有测试的话第一次启用 TLS 就会在启动时崩。

## 回滚

```bash
git checkout main   # 或上游 tag
```

改动不涉及数据库 schema 变更，`proxy_tls_cert` 字段有 `#[serde(default)]`，旧配置文件可直接读。
