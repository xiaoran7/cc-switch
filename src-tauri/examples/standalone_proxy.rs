use cc_switch_lib::proxy::server::ProxyServer;
use cc_switch_lib::proxy::types::ProxyConfig;
use cc_switch_lib::Database;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 简单输出日志到终端
    struct SimpleLogger;
    impl log::Log for SimpleLogger {
        fn enabled(&self, metadata: &log::Metadata) -> bool {
            metadata.level() <= log::Level::Info
        }
        fn log(&self, record: &log::Record) {
            if self.enabled(record.metadata()) {
                println!("[{}] {}", record.level(), record.args());
            }
        }
        fn flush(&self) {}
    }
    let _ = log::set_boxed_logger(Box::new(SimpleLogger));
    log::set_max_level(log::LevelFilter::Info);

    println!("[STANDALONE] Starting proxy test...");

    let db = Arc::new(Database::init()?);
    println!("[STANDALONE] Database connected successfully.");

    let host = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("HOST").ok())
        .unwrap_or_else(|| "127.0.0.1".to_string());

    let port: u16 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .or_else(|| std::env::var("PORT").ok().and_then(|s| s.parse().ok()))
        .unwrap_or(15721);

    let config = ProxyConfig {
        listen_address: host,
        listen_port: port,
        ..ProxyConfig::default()
    };

    let server = ProxyServer::new(config, db, None);
    let info = server
        .start()
        .await
        .map_err(|e| format!("Server start failed: {e}"))?;
    println!(
        "[STANDALONE] Server started: {} on port {}",
        info.address, info.port
    );
    println!("[STANDALONE] TLS Termination is ACTIVE. Ready for verification requests.");

    // 保持运行，等待外部终止信号
    println!("[STANDALONE] Running in foreground, press Ctrl+C or kill to exit.");
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(3600)).await;
    }
}
