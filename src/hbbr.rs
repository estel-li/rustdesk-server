use clap::App;
#[path = "admin_cli.rs"]
mod admin_cli;
mod common;
mod metrics;
mod relay_server;
use flexi_logger::*;
use hbb_common::{config::RELAY_PORT, ResultType};
use relay_server::*;
mod version;

fn main() -> ResultType<()> {
    let _logger = Logger::try_with_env_or_str("info")?
        .log_to_stdout()
        .format(opt_format)
        .write_mode(WriteMode::Async)
        .start()?;
    let args = format!(
        "-p, --port=[NUMBER(default={RELAY_PORT})] 'Sets the listening port'
        -k, --key=[KEY] 'Only allow the client with the same key'
        , --metrics-bind=[ADDR] 'Bind Prometheus metrics endpoint, e.g. 127.0.0.1:21121 (disabled if empty)'
        , --admin-socket=[PATH] 'CE-M0-7: Unix domain socket path for admin CLI (empty = disable UDS)'
        , --admin-tcp=[ADDR] 'CE-M0-7: TCP loopback addr for admin CLI; Windows fallback only'
        , --admin-token-file=[PATH] 'CE-M0-7: Path to persist admin token (mode 0640)'
        , --admin-disable=[Y/N] 'CE-M0-7: Set Y to fully disable admin CLI'
        ",
    );
    let matches = App::new("hbbr")
        .version(version::VERSION)
        .author("Purslane Ltd. <info@rustdesk.com>")
        .about("RustDesk Relay Server")
        .args_from_usage(&args)
        .get_matches();
    if let Ok(v) = ini::Ini::load_from_file(".env") {
        if let Some(section) = v.section(None::<String>) {
            section.iter().for_each(|(k, v)| std::env::set_var(k, v));
        }
    }
    // CE-M0-7: clap 解出来的 admin-* / metrics-bind 也写回 env,与 init_args 行为对齐。
    for key in [
        "admin-socket",
        "admin-tcp",
        "admin-token-file",
        "admin-disable",
    ] {
        if let Some(v) = matches.value_of(key) {
            std::env::set_var(key.to_uppercase().replace('_', "-"), v);
        }
    }
    let mut port = RELAY_PORT;
    if let Ok(v) = std::env::var("PORT") {
        let v: i32 = v.parse().unwrap_or_default();
        if v > 0 {
            port = v + 1;
        }
    }
    // CE-M0-3: --metrics-bind 优先,空时回退 METRICS_BIND 环境变量;两者都空则不启用。
    let metrics_bind = matches
        .value_of("metrics-bind")
        .map(|s| s.to_owned())
        .or_else(|| std::env::var("METRICS_BIND").ok())
        .unwrap_or_default();
    if !metrics_bind.is_empty() {
        let addr: std::net::SocketAddr = metrics_bind.parse().map_err(|e| {
            hbb_common::anyhow::anyhow!("invalid --metrics-bind {}: {}", metrics_bind, e)
        })?;
        metrics::install_recorder(addr, "hbbr")?;
    }
    start(
        matches.value_of("port").unwrap_or(&port.to_string()),
        matches
            .value_of("key")
            .unwrap_or(&std::env::var("KEY").unwrap_or_default()),
    )?;
    Ok(())
}
