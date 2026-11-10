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
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

/// 从 X.509 叶子证书中提取到的身份标识（SAN 与 Subject CN），用于本地核验与日志诊断。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CertIdentities {
    pub dns_names: Vec<String>,
    pub ip_addrs: Vec<IpAddr>,
    pub common_names: Vec<String>,
}

impl CertIdentities {
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        for dns in &self.dns_names {
            parts.push(format!("DNS:{dns}"));
        }
        for ip in &self.ip_addrs {
            parts.push(format!("IP:{ip}"));
        }
        if !self.common_names.is_empty() {
            parts.push(format!("CN={}", self.common_names.join(",")));
        }
        if parts.is_empty() {
            "(无 SAN/CN)".to_string()
        } else {
            parts.join(", ")
        }
    }

    pub fn matches_host(&self, host: &str) -> bool {
        let normalized = normalize_host(host);
        if normalized.is_empty() {
            return false;
        }
        if let Ok(ip) = normalized.parse::<IpAddr>() {
            let ip = ip.to_canonical();
            if !self.ip_addrs.is_empty() || !self.dns_names.is_empty() {
                return self.ip_addrs.iter().any(|san| san.to_canonical() == ip);
            }
            return self
                .common_names
                .iter()
                .any(|cn| normalize_host(cn) == normalized);
        }

        if !self.dns_names.is_empty() || !self.ip_addrs.is_empty() {
            return self
                .dns_names
                .iter()
                .any(|pattern| dns_pattern_matches(pattern, &normalized));
        }

        self.common_names
            .iter()
            .any(|cn| dns_pattern_matches(cn, &normalized))
    }
}

fn normalize_host(host: &str) -> String {
    host.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

fn dns_pattern_matches(pattern: &str, host: &str) -> bool {
    let pattern = normalize_host(pattern);
    if pattern == host {
        return true;
    }
    if let Some(suffix) = pattern.strip_prefix("*.") {
        if !suffix.contains('.') {
            return false;
        }
        if let Some(rest) = host.strip_suffix(suffix) {
            if let Some(label) = rest.strip_suffix('.') {
                return !label.is_empty() && !label.contains('.');
            }
        }
    }
    false
}

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
    /// `host` 用于拼默认文件名与 SAN 校验，IPv6 的方括号会被去掉。
    pub fn resolve(explicit: Option<&str>, data_dir: &Path, host: &str) -> Option<Self> {
        let normalized_host = normalize_host(host);
        let needs_san_check = !crate::services::proxy::is_unspecified_host(&normalized_host);

        if let Some(cert_path) = explicit.map(str::trim).filter(|p| !p.is_empty()) {
            let cert = PathBuf::from(cert_path);
            // 显式给了证书路径时，私钥默认同目录同名，只把 .crt/.pem 换成 .key
            let key = cert.with_extension("key");
            if cert.is_file() && key.is_file() {
                if needs_san_check {
                    if let Some(ids) = inspect_cert_file(&cert) {
                        if !ids.matches_host(&normalized_host) {
                            log::warn!(
                                "[ProxyTLS] 显式设置的证书 {} 未覆盖目标主机 {}（证书内容: {}）",
                                cert.display(),
                                normalized_host,
                                ids.summary()
                            );
                        }
                    }
                }
                return Some(Self { cert, key });
            }
            log::warn!(
                "[ProxyTLS] 设置里的证书不可用: cert={} key={}（两者都需存在）",
                cert.display(),
                key.display()
            );
        }

        let dir = data_dir.join("tls");
        let cert = dir.join(format!("{normalized_host}.crt"));
        let key = dir.join(format!("{normalized_host}.key"));
        if cert.is_file() && key.is_file() {
            if needs_san_check {
                if let Some(ids) = inspect_cert_file(&cert) {
                    if !ids.matches_host(&normalized_host) {
                        log::warn!(
                            "[ProxyTLS] 证书 {} 未覆盖目标主机 {}（证书内容: {}）",
                            cert.display(),
                            normalized_host,
                            ids.summary()
                        );
                    }
                }
            }
            return Some(Self { cert, key });
        }

        // 自适应扫描：当约定文件名不直接匹配时（如监听或 client_host 是 100.x IP，而自签证书
        // 以 `<domain>.ts.net.crt` 命名且在 SAN 里同时签了该 IP），校验 SAN/CN 确实覆盖目标主机后再采用。
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let mut candidates: Vec<(PathBuf, PathBuf)> = entries
                .flatten()
                .filter_map(|entry| {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("crt") {
                        return None;
                    }
                    let key_path = path.with_extension("key");
                    key_path.is_file().then_some((path, key_path))
                })
                .collect();
            candidates.sort_by(|a, b| a.0.cmp(&b.0));

            if needs_san_check {
                let mut matched = Vec::new();
                let mut mismatched = Vec::new();
                for (c, k) in candidates {
                    if let Some(ids) = inspect_cert_file(&c) {
                        if ids.matches_host(&normalized_host) {
                            matched.push((c, k, ids));
                        } else {
                            mismatched.push((c, ids));
                        }
                    }
                }
                if let Some((c, k, ids)) = matched
                    .iter()
                    .find(|(c, _, _)| c.to_string_lossy().contains(".ts.net"))
                    .or_else(|| matched.first())
                {
                    log::info!(
                        "[ProxyTLS] 自动匹配到覆盖 {} 的证书对: {} ({})",
                        normalized_host,
                        c.display(),
                        ids.summary()
                    );
                    return Some(Self {
                        cert: c.clone(),
                        key: k.clone(),
                    });
                }
                if !mismatched.is_empty() {
                    let details = mismatched
                        .iter()
                        .map(|(c, ids)| format!("{} [{}]", c.display(), ids.summary()))
                        .collect::<Vec<_>>()
                        .join("; ");
                    log::warn!(
                        "[ProxyTLS] tls/ 下存在证书但均未覆盖目标主机 {}，拒绝回退使用: {}",
                        normalized_host,
                        details
                    );
                }
            } else if candidates.len() == 1 {
                let (c, k) = candidates.remove(0);
                let summary = inspect_cert_file(&c)
                    .map(|ids| ids.summary())
                    .unwrap_or_else(|| "无法解析".to_string());
                log::info!(
                    "[ProxyTLS] 自动匹配到 tls 目录下的唯一证书对: {} ({})",
                    c.display(),
                    summary
                );
                return Some(Self { cert: c, key: k });
            } else if let Some((c, k)) = candidates
                .into_iter()
                .find(|(c, _)| c.to_string_lossy().contains(".ts.net"))
            {
                let summary = inspect_cert_file(&c)
                    .map(|ids| ids.summary())
                    .unwrap_or_else(|| "无法解析".to_string());
                log::info!(
                    "[ProxyTLS] 自动匹配到 tailnet 证书对: {} ({})",
                    c.display(),
                    summary
                );
                return Some(Self { cert: c, key: k });
            }
        }

        log::debug!(
            "[ProxyTLS] 未找到覆盖 {} 的证书，尝试过: {} 与 {}",
            normalized_host,
            cert.display(),
            key.display()
        );
        None
    }
}

pub(crate) fn inspect_cert_file(path: &Path) -> Option<CertIdentities> {
    let certs = load_certs(path).ok()?;
    let leaf = certs.first()?;
    parse_leaf_cert_identities(leaf.as_ref())
}

fn read_der_tlv(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, mut rest) = input.split_first()?;
    let (&len_byte, after_len_byte) = rest.split_first()?;
    rest = after_len_byte;
    let len: usize = if len_byte < 0x80 {
        len_byte as usize
    } else {
        let num_bytes = (len_byte & 0x7F) as usize;
        if num_bytes == 0 || num_bytes > 4 || rest.len() < num_bytes {
            return None;
        }
        let (len_bytes, after_len) = rest.split_at(num_bytes);
        rest = after_len;
        let mut acc: usize = 0;
        for &b in len_bytes {
            acc = (acc << 8) | (b as usize);
        }
        acc
    };
    if rest.len() < len {
        return None;
    }
    let (value, remaining) = rest.split_at(len);
    Some((tag, value, remaining))
}

fn decode_der_string(tag: u8, bytes: &[u8]) -> Option<String> {
    match tag {
        // UTF8String (0x0C), PrintableString (0x13), T61String (0x14), IA5String (0x16), UniversalString/VisibleString
        0x0C | 0x13 | 0x14 | 0x16 | 0x1A => std::str::from_utf8(bytes).ok().map(str::to_string),
        // BMPString (UTF-16BE)
        0x1E if bytes.len() % 2 == 0 => {
            let units: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16(&units).ok()
        }
        _ => None,
    }
}

fn extract_subject_cns(subject_der: &[u8], out: &mut CertIdentities) {
    const OID_COMMON_NAME: &[u8] = &[0x55, 0x04, 0x03];
    let mut rdns = subject_der;
    while let Some((rdn_tag, rdn_val, rest_rdns)) = read_der_tlv(rdns) {
        rdns = rest_rdns;
        if rdn_tag != 0x31 {
            continue;
        }
        let mut atvs = rdn_val;
        while let Some((atv_tag, atv_val, rest_atvs)) = read_der_tlv(atvs) {
            atvs = rest_atvs;
            if atv_tag != 0x30 {
                continue;
            }
            let Some((oid_tag, oid_val, after_oid)) = read_der_tlv(atv_val) else {
                continue;
            };
            if oid_tag == 0x06 && oid_val == OID_COMMON_NAME {
                if let Some((val_tag, val_bytes, _)) = read_der_tlv(after_oid) {
                    if let Some(cn) = decode_der_string(val_tag, val_bytes) {
                        out.common_names.push(cn);
                    }
                }
            }
        }
    }
}

fn extract_san_extension(extn_der: &[u8], out: &mut CertIdentities) {
    let Some((seq_tag, seq_val, _)) = read_der_tlv(extn_der) else {
        return;
    };
    if seq_tag != 0x30 {
        return;
    }
    let mut cursor = seq_val;
    while let Some((tag, val, rest)) = read_der_tlv(cursor) {
        cursor = rest;
        match tag {
            // [2] dNSName IMPLICIT IA5String
            0x82 => {
                if let Ok(dns) = std::str::from_utf8(val) {
                    let normalized = normalize_host(dns);
                    if !normalized.is_empty() {
                        out.dns_names.push(normalized);
                    }
                }
            }
            // [7] iPAddress IMPLICIT OCTET STRING
            0x87 => match val.len() {
                4 => {
                    let octets: [u8; 4] = [val[0], val[1], val[2], val[3]];
                    out.ip_addrs.push(IpAddr::V4(octets.into()));
                }
                16 => {
                    if let Ok(octets) = <[u8; 16]>::try_from(val) {
                        let ip: std::net::Ipv6Addr = octets.into();
                        out.ip_addrs.push(IpAddr::V6(ip).to_canonical());
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
}

fn extract_extensions(extensions_explicit_der: &[u8], out: &mut CertIdentities) {
    const OID_SUBJECT_ALT_NAME: &[u8] = &[0x55, 0x1D, 0x11];
    let Some((seq_tag, seq_val, _)) = read_der_tlv(extensions_explicit_der) else {
        return;
    };
    if seq_tag != 0x30 {
        return;
    }
    let mut cursor = seq_val;
    while let Some((ext_tag, ext_val, rest)) = read_der_tlv(cursor) {
        cursor = rest;
        if ext_tag != 0x30 {
            continue;
        }
        let Some((oid_tag, oid_val, after_oid)) = read_der_tlv(ext_val) else {
            continue;
        };
        if oid_tag != 0x06 || oid_val != OID_SUBJECT_ALT_NAME {
            continue;
        }
        let mut ext_rest = after_oid;
        if let Some((next_tag, _, after_critical)) = read_der_tlv(ext_rest) {
            if next_tag == 0x01 {
                ext_rest = after_critical;
            }
        }
        if let Some((octet_tag, octet_val, _)) = read_der_tlv(ext_rest) {
            if octet_tag == 0x04 {
                extract_san_extension(octet_val, out);
            }
        }
    }
}

pub(crate) fn parse_leaf_cert_identities(der: &[u8]) -> Option<CertIdentities> {
    let (cert_tag, cert_val, _) = read_der_tlv(der)?;
    if cert_tag != 0x30 {
        return None;
    }
    let (tbs_tag, mut cursor, _) = read_der_tlv(cert_val)?;
    if tbs_tag != 0x30 {
        return None;
    }

    // Optional [0] EXPLICIT version
    if let Some((tag, _, rest)) = read_der_tlv(cursor) {
        if tag == 0xA0 {
            cursor = rest;
        }
    }
    // serialNumber (INTEGER), signature (SEQUENCE), issuer (SEQUENCE), validity (SEQUENCE)
    for expected_tag in [0x02u8, 0x30, 0x30, 0x30] {
        let (tag, _, rest) = read_der_tlv(cursor)?;
        if tag != expected_tag {
            return None;
        }
        cursor = rest;
    }

    let mut out = CertIdentities::default();
    // subject (Name SEQUENCE)
    let (subject_tag, subject_val, rest) = read_der_tlv(cursor)?;
    if subject_tag != 0x30 {
        return None;
    }
    extract_subject_cns(subject_val, &mut out);
    cursor = rest;

    // subjectPublicKeyInfo (SEQUENCE)
    let (spki_tag, _, rest) = read_der_tlv(cursor)?;
    if spki_tag != 0x30 {
        return None;
    }
    cursor = rest;

    // Optional fields until [3] EXPLICIT extensions
    while let Some((tag, val, rest)) = read_der_tlv(cursor) {
        cursor = rest;
        if tag == 0xA3 {
            extract_extensions(val, &mut out);
            break;
        }
    }

    Some(out)
}

/// 读 PEM 证书链。返回全部证书，第一张是叶子证书。
fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, String> {
    let file =
        std::fs::File::open(path).map_err(|e| format!("打开证书失败 {}: {e}", path.display()))?;
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
    let file =
        std::fs::File::open(path).map_err(|e| format!("打开私钥失败 {}: {e}", path.display()))?;
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
    if let Some(ids) = certs
        .first()
        .and_then(|leaf| parse_leaf_cert_identities(leaf.as_ref()))
    {
        log::info!(
            "[ProxyTLS] 已加载叶子证书 {}: {}",
            material.cert.display(),
            ids.summary()
        );
    }

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

        assert!(!crate::services::proxy::is_loopback_host("100.64.0.1"));
        assert!(!crate::services::proxy::is_loopback_host("198.51.100.1"));
        assert!(!crate::services::proxy::is_loopback_host(
            "my-node.example.ts.net"
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
            crate::services::proxy::proxy_origin("100.64.0.1", 15721),
            "https://100.64.0.1:15721"
        );
        assert_eq!(
            crate::services::proxy::proxy_origin("my-node.example.ts.net", 15721),
            "https://my-node.example.ts.net:15721"
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
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-keyout",
                key.to_str()?,
                "-out",
                crt.to_str()?,
                "-days",
                "1",
                "-subj",
                "/CN=probe.test",
                // 默认生成的 x509 带 CA:TRUE，rustls 会拒绝「把 CA 当叶子证书」，
                // 这里明确标成终端实体证书。
                "-addext",
                "subjectAltName=DNS:probe.test",
                "-addext",
                "basicConstraints=critical,CA:FALSE",
                "-addext",
                "keyUsage=critical,digitalSignature,keyEncipherment",
                "-addext",
                "extendedKeyUsage=serverAuth",
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

        let acceptor = build_acceptor(&TlsMaterial {
            cert: crt.clone(),
            key,
        })
        .expect("构造 acceptor");

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
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
        let mut tls = connector
            .connect(server_name, tcp)
            .await
            .expect("客户端握手");

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
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
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

    fn make_self_signed_with_sans(
        dir: &std::path::Path,
        stem: &str,
        cn: &str,
        san_ext: &str,
    ) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
        let crt = dir.join(format!("{stem}.crt"));
        let key = dir.join(format!("{stem}.key"));
        let status = std::process::Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-keyout",
                key.to_str()?,
                "-out",
                crt.to_str()?,
                "-days",
                "1",
                "-subj",
                &format!("/CN={cn}"),
                "-addext",
                &format!("subjectAltName={san_ext}"),
                "-addext",
                "basicConstraints=critical,CA:FALSE",
                "-addext",
                "keyUsage=critical,digitalSignature,keyEncipherment",
                "-addext",
                "extendedKeyUsage=serverAuth",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()?;
        status.success().then_some((crt, key))
    }

    #[test]
    fn cert_san_inspection_and_tier3_fallback_enforce_host_match() {
        let dir = std::env::temp_dir().join("ccswitch-tls-san-check");
        let _ = std::fs::remove_dir_all(&dir);
        let tls_dir = dir.join("tls");
        std::fs::create_dir_all(&tls_dir).unwrap();

        let Some((ts_crt, ts_key)) = make_self_signed_with_sans(
            &tls_dir,
            "my-node.tail123.ts.net",
            "my-node.tail123.ts.net",
            "DNS:my-node.tail123.ts.net,IP:100.64.0.10",
        ) else {
            eprintln!("跳过：系统没有 openssl");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };

        let ids = inspect_cert_file(&ts_crt).expect("parse leaf SANs");
        assert!(ids.matches_host("my-node.tail123.ts.net"));
        assert!(ids.matches_host("100.64.0.10"));
        assert!(!ids.matches_host("100.64.0.99"));
        assert!(!ids.matches_host("other-node.tail123.ts.net"));

        // Tier 3 fallback succeeds when the cert SAN covers the target IP
        let resolved_ip = TlsMaterial::resolve(None, &dir, "100.64.0.10")
            .expect("matching SAN IP should resolve");
        assert_eq!(resolved_ip.cert, ts_crt);
        assert_eq!(resolved_ip.key, ts_key);

        // Tier 3 fallback MUST refuse a cert whose SAN does not cover the target host
        assert!(TlsMaterial::resolve(None, &dir, "100.64.0.99").is_none());
        assert!(TlsMaterial::resolve(None, &dir, "other-node.tail123.ts.net").is_none());

        // Wildcard SAN matches one subdomain label only
        let wildcard = CertIdentities {
            dns_names: vec!["*.tail123.ts.net".to_string()],
            ip_addrs: vec![],
            common_names: vec![],
        };
        assert!(wildcard.matches_host("alpha.tail123.ts.net"));
        assert!(!wildcard.matches_host("a.b.tail123.ts.net"));
        assert!(!wildcard.matches_host("tail123.ts.net"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
