// https://tools.ietf.org/rfc/rfc5128.txt
// https://blog.csdn.net/bytxl/article/details/44344855

use flexi_logger::*;
use hbb_common::{bail, config::RENDEZVOUS_PORT, ResultType};
use hbbs::{common::*, *};

const RMEM: usize = 0;

fn main() -> ResultType<()> {
    let _logger = Logger::try_with_env_or_str("info")?
        .log_to_stdout()
        .format(opt_format)
        .write_mode(WriteMode::Async)
        .start()?;
    let args = format!(
        "-c --config=[FILE] +takes_value 'Sets a custom config file'
        -p, --port=[NUMBER(default={RENDEZVOUS_PORT})] 'Sets the listening port'
        -s, --serial=[NUMBER(default=0)] 'Sets configure update serial number'
        -R, --rendezvous-servers=[HOSTS] 'Sets rendezvous servers, separated by comma'
        -u, --software-url=[URL] 'Sets download url of RustDesk software of newest version'
        -r, --relay-servers=[HOST] 'Sets the default relay servers, separated by comma'
        -M, --rmem=[NUMBER(default={RMEM})] 'Sets UDP recv buffer size, set system rmem_max first, e.g., sudo sysctl -w net.core.rmem_max=52428800. vi /etc/sysctl.conf, net.core.rmem_max=52428800, sudo sysctl –p'
        , --mask=[MASK] 'Determine if the connection comes from LAN, e.g. 192.168.0.0/16'
        , --metrics-bind=[ADDR] 'Bind Prometheus metrics endpoint, e.g. 127.0.0.1:21120 (disabled if empty)'
        , --admin-socket=[PATH] 'CE-M0-7: Unix domain socket path for admin CLI (empty = disable UDS)'
        , --admin-tcp=[ADDR] 'CE-M0-7: TCP loopback addr for admin CLI; Windows fallback, must be 127.0.0.1/::1'
        , --admin-token-file=[PATH] 'CE-M0-7: Path to persist admin token (mode 0640)'
        , --admin-disable=[Y/N] 'CE-M0-7: Set Y to fully disable admin CLI'
        -k, --key=[KEY] 'Only allow the client with the same key'",
    );
    init_args(&args, "hbbs", "RustDesk ID/Rendezvous Server");
    let port = get_arg_or("port", RENDEZVOUS_PORT.to_string()).parse::<i32>()?;
    if port < 3 {
        bail!("Invalid port");
    }
    let rmem = get_arg("rmem").parse::<usize>().unwrap_or(RMEM);
    let serial: i32 = get_arg("serial").parse().unwrap_or(0);
    // CE-M0-3: 若指定 --metrics-bind,则启动 Prometheus 独立端口;空字符串/未传 = 关闭。
    let metrics_bind = get_arg("metrics-bind");
    if !metrics_bind.is_empty() {
        let addr: std::net::SocketAddr = metrics_bind.parse().map_err(|e| {
            hbb_common::anyhow::anyhow!("invalid --metrics-bind {}: {}", metrics_bind, e)
        })?;
        hbbs::metrics::install_recorder(addr, "hbbs")?;
    }
    crate::common::check_software_update();
    RendezvousServer::start(port, serial, &get_arg_or("key", "-".to_owned()), rmem)?;
    Ok(())
}
