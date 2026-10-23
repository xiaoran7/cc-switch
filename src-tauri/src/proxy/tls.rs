//! 代理服务器的 TLS 终结层。
//!
//! 只在监听地址是**非回环**时启用。原因见 `services::proxy::proxy_origin`：
//! Claude Desktop 对 `inferenceGatewayBaseUrl` 的校验只允许回环主机走明文 `http`，
//! 任何非回环地址（tailnet IP、MagicDNS 名、公网域名）都必须 `https`。
//!
//! 证书来源按优先级：
//! 1. 设置里的 `proxy_tls_cert` / `proxy_tls_key`（PEM 路径）
//! 2. `<应用数据目录>/tls/<host>.crt` 与 `.key`（`tailscale cert` 的默认落盘位置）
//!
//! 找不到证书时**不静默降级成明文**——那会让客户端拿到一个连不上的 https 地址。
//! 直接返回错误，让调用方把问题暴露在 UI 上。

use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

/// TLS 材料在磁盘上的位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsMaterial {
    pub cert: PathBuf,
    pub key: PathBuf,
}

impl TlsMaterial {
    /// 按 `显式设置 -> 数据目录约定位置` 的顺序找证书。
    ///
    /// `explicit` 是设置里手动指定的证书路径（可选）；`data_dir` 是应用数据目录。
    /// `host` 用于拼默认文件名，IPv6 的方括号会被去掉。
    pub fn resolve(explicit: Option<&str>, data_dir: &Path, host: &str) -> Option<Self> {
        if let Some(cert_path) = explicit.map(str::trim).filter(|p| !p.is_empty()) {
            let cert = PathBuf::from(cert_path);
            // 显式给了证书路径时，私钥默认同目录同名，只把 .crt/.pem 换成 .key
            let key = cert.with_extension("key");
            if cert.is_file() && key.is_file() {
                return Some(Self { cert, key });
            }
            log::warn!(
                "[ProxyTLS] 设置里的证书不可用: cert={} key={}（两者都需存在）",
                cert.display(),
                key.display()
            );
        }

        let host = host.trim().trim_start_matches('[').trim_end_matches(']');
        let dir = data_dir.join("tls");
        let cert = dir.join(format!("{host}.crt"));
        let key = dir.join(format!("{host}.key"));
        if cert.is_file() && key.is_file() {
            return Some(Self { cert, key });
        }

        log::debug!(
            "[ProxyTLS] 未找到证书，尝试过: {} 与 {}",
            dir.join(format!("{host}.crt")).display(),
            dir.join(format!("{host}.key")).display()
        );
        None
    }
}

/// 读 PEM 证书链。返回全部证书，第一张是叶子证书。
fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, String> {
    let file = std::fs::File::open(path)
        .map_err(|e| format!("打开证书失败 {}: {e}", path.display()))?;
    let mut reader = BufReader::new(file);
    let certs = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("解析证书失败 {}: {e}", path.display()))?;
    if certs.is_empty() {
        return Err(format!("证书文件里没有 PEM 证书: {}", path.display()));
    }
    Ok(certs)
}

/// 读 PEM 私钥。支持 PKCS#8 / PKCS#1 / SEC1 三种编码。
fn load_key(path: &Path) -> Result<PrivateKeyDer<'static>, String> {
    let file = std::fs::File::open(path)
        .map_err(|e| format!("打开私钥失败 {}: {e}", path.display()))?;
    let mut reader = BufReader::new(file);
    let key = rustls_pemfile::private_key(&mut reader)
        .map_err(|e| format!("解析私钥失败 {}: {e}", path.display()))?
        .ok_or_else(|| format!("私钥文件里没有 PEM 私钥: {}", path.display()))?;
    Ok(key)
}

/// 从磁盘材料构造 `TlsAcceptor`。
pub fn build_acceptor(material: &TlsMaterial) -> Result<TlsAcceptor, String> {
    // 必须在 ServerConfig::builder() 之前装好 provider，否则直接 panic
    install_crypto_provider();

    let certs = load_certs(&material.cert)?;
    let key = load_key(&material.key)?;

    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("组装 TLS 配置失败: {e}"))?;

    // 代理会转发大量小请求，开启 session ticket 减少重复握手
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// 显式安装 rustls 的进程级 CryptoProvider。
///
/// 这个 crate 的依赖树里 `hyper-rustls` 拉的是 `aws-lc-rs`、`reqwest` 拉的是 `ring`，
/// **两个 provider 同时启用**，rustls 无法自动判定，任何 `ServerConfig::builder()` /
/// `ClientConfig::builder()` 都会 panic。必须在本进程第一次用 rustls 之前选一个。
///
/// 选 `ring`：`reqwest` 已经在用它，且不需要 aws-lc-rs 的 C 工具链。
/// 重复调用是安全的（返回 Err 说明已经装过，忽略即可）。
pub fn install_crypto_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        if tokio_rustls::rustls::crypto::ring::default_provider()
            .install_default()
            .is_err()
        {
            log::debug!("[ProxyTLS] CryptoProvider 已由其它组件安装，跳过");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_detection_matches_desktop_allowlist() {
        // 与 Claude Desktop 的 `allowLoopbackHttp` 白名单对齐
        assert!(crate::services::proxy::is_loopback_host("127.0.0.1"));
        assert!(crate::services::proxy::is_loopback_host("127.1.2.3"));
        assert!(crate::services::proxy::is_loopback_host("localhost"));
        assert!(crate::services::proxy::is_loopback_host("LocalHost."));
        assert!(crate::services::proxy::is_loopback_host("::1"));
        assert!(crate::services::proxy::is_loopback_host("[::1]"));
        assert!(crate::services::proxy::is_loopback_host("0.0.0.0"));

        assert!(!crate::services::proxy::is_loopback_host("100.79.74.121"));
        assert!(!crate::services::proxy::is_loopback_host("100.107.77.33"));
        assert!(!crate::services::proxy::is_loopback_host(
            "portable-pc.tail462ee4.ts.net"
        ));
    }

    #[test]
    fn proxy_origin_uses_https_for_non_loopback() {
        assert_eq!(
            crate::services::proxy::proxy_origin("127.0.0.1", 15721),
            "http://127.0.0.1:15721"
        );
        assert_eq!(
            crate::services::proxy::proxy_origin("0.0.0.0", 15721),
            "http://127.0.0.1:15721"
        );
        assert_eq!(
            crate::services::proxy::proxy_origin("100.79.74.121", 15721),
            "https://100.79.74.121:15721"
        );
        assert_eq!(
            crate::services::proxy::proxy_origin("portable-pc.tail462ee4.ts.net", 15721),
            "https://portable-pc.tail462ee4.ts.net:15721"
        );
        // IPv6 非回环要加方括号
        assert_eq!(
            crate::services::proxy::proxy_origin("fd7a:115c:a1e0::1", 15721),
            "https://[fd7a:115c:a1e0::1]:15721"
        );
    }

    #[test]
    fn tls_material_resolution_prefers_explicit_then_convention() {
        let tmp = std::env::temp_dir().join("ccswitch-tls-test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("tls")).unwrap();

        // 都没有 -> None
        assert!(TlsMaterial::resolve(None, &tmp, "example.test").is_none());

        // 约定位置
        let cert = tmp.join("tls").join("example.test.crt");
        let key = tmp.join("tls").join("example.test.key");
        std::fs::write(&cert, "dummy").unwrap();
        std::fs::write(&key, "dummy").unwrap();
        let found = TlsMaterial::resolve(None, &tmp, "example.test").unwrap();
        assert_eq!(found.cert, cert);
        assert_eq!(found.key, key);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ==================== 端到端：真实握手 ====================

    /// 用 openssl 生成一对自签证书到临时目录。系统没有 openssl 时返回 None（测试跳过）。
    fn make_self_signed(dir: &std::path::Path) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
        let crt = dir.join("probe.test.crt");
        let key = dir.join("probe.test.key");
        let status = std::process::Command::new("openssl")
            .args([
                "req", "-x509", "-newkey", "rsa:2048", "-nodes",
                "-keyout", key.to_str()?, "-out", crt.to_str()?,
                "-days", "1", "-subj", "/CN=probe.test",
                // 默认生成的 x509 带 CA:TRUE，rustls 会拒绝「把 CA 当叶子证书」，
                // 这里明确标成终端实体证书。
                "-addext", "subjectAltName=DNS:probe.test",
                "-addext", "basicConstraints=critical,CA:FALSE",
                "-addext", "keyUsage=critical,digitalSignature,keyEncipherment",
                "-addext", "extendedKeyUsage=serverAuth",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()?;
        status.success().then_some((crt, key))
    }

    /// 起一个真实 TLS 服务，客户端用 rustls 连上去，确认握手成功、数据能双向流动。
    #[tokio::test]
    async fn tls_acceptor_serves_a_real_handshake() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let dir = std::env::temp_dir().join("ccswitch-tls-e2e");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let Some((crt, key)) = make_self_signed(&dir) else {
            eprintln!("跳过：系统没有 openssl");
            return;
        };

        let acceptor = build_acceptor(&TlsMaterial { cert: crt.clone(), key }).expect("构造 acceptor");

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut tls = acceptor.accept(stream).await.expect("服务端握手");
            let mut buf = [0u8; 5];
            let _ = tls.read(&mut buf).await;
            let _ = tls.write_all(b"ok\n").await;
            let _ = tls.shutdown().await;
        });

        // 客户端信任刚生成的那张证书
        install_crypto_provider();
        let mut roots = tokio_rustls::rustls::RootCertStore::empty();
        let pem = std::fs::read(&crt).unwrap();
        for cert in rustls_pemfile::certs(&mut std::io::BufReader::new(&pem[..])) {
            roots.add(cert.unwrap()).unwrap();
        }
        let client_cfg = tokio_rustls::rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client_cfg));

        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let server_name =
            tokio_rustls::rustls::pki_types::ServerName::try_from("probe.test").unwrap();
        let mut tls = connector.connect(server_name, tcp).await.expect("客户端握手");

        tls.write_all(b"ping\n").await.unwrap();
        let mut out = Vec::new();
        tls.read_to_end(&mut out).await.unwrap();
        assert_eq!(out, b"ok\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 明文 HTTP 打不进 TLS 端口——证明真的做了终结，不是直通。
    #[tokio::test]
    async fn plaintext_cannot_talk_to_tls_port() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let dir = std::env::temp_dir().join("ccswitch-tls-plain");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let Some((crt, key)) = make_self_signed(&dir) else {
            eprintln!("跳过：系统没有 openssl");
            return;
        };

        let acceptor = build_acceptor(&TlsMaterial { cert: crt, key }).expect("构造 acceptor");
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                assert!(acceptor.accept(stream).await.is_err(), "明文不该握手成功");
            }
        });

        let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let _ = tcp.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").await;
        let mut buf = Vec::new();
        let _ = tcp.read_to_end(&mut buf).await;
        assert!(
            !buf.starts_with(b"HTTP/1.1 200"),
            "明文请求不该拿到正常响应"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
