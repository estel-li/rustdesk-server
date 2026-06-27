// CE-M0-3 integration smoke test: 验证 metrics 模块自洽。
// 不依赖完整 hbbs/hbbr 启动,仅安装 recorder + 自增几个 metric,
// 然后用 std::net::TcpStream 抓取 `/metrics`,断言关键文本存在。
//
// 注意:`metrics::set_global_recorder` 是进程级一次性 API,所以本文件
// 只能容纳一个 `#[test]`(否则后续 install 会因 global 已设置而失败)。

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

use hbbs::metrics as ce_metrics;

/// 抓取 `http://addr/metrics` 文本(含响应行/头)。
fn http_get_metrics(addr: SocketAddr) -> std::io::Result<String> {
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(3))?;
    s.set_read_timeout(Some(Duration::from_secs(3)))?;
    s.set_write_timeout(Some(Duration::from_secs(3)))?;
    let req = format!(
        "GET /metrics HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        addr
    );
    s.write_all(req.as_bytes())?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
}

#[test]
fn metrics_smoke() {
    // (A) helpers 在 install 之前调用应不 panic(零端口、无 listener 路径)
    ce_metrics::inc_register("peer");
    ce_metrics::inc_punch_hole("udp");
    ce_metrics::inc_punch_hole_result("ok");
    ce_metrics::inc_relay_assign();
    ce_metrics::set_peers_online(0);
    ce_metrics::set_peermap_entries(0);
    ce_metrics::observe_access_check("allow", 0.001);
    ce_metrics::inc_ws_active(1);
    ce_metrics::inc_ws_active(-1);
    ce_metrics::inc_ip_blocked();

    // (B) 端口占用 -> install_recorder 应返回 Err
    let busy = TcpListener::bind("127.0.0.1:0").unwrap();
    let busy_addr = busy.local_addr().unwrap();
    let res = ce_metrics::install_recorder(busy_addr, "hbbs");
    assert!(res.is_err(), "expected install failure on busy port");
    drop(busy);

    // (C) happy path:绑随机端口安装 recorder,自增 counter,scrape /metrics
    let port = free_port();
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    ce_metrics::install_recorder(addr, "hbbs").expect("install");
    ce_metrics::inc_register("peer");
    ce_metrics::inc_register("peer");
    ce_metrics::inc_register("peer");
    // 给 exporter 时间汇总
    std::thread::sleep(Duration::from_millis(300));
    let body = http_get_metrics(addr).expect("http get");
    assert!(body.contains("HTTP/1.1 200"), "no 200 response: {body}");
    assert!(
        body.contains("hbbs_build_info"),
        "missing build_info: {body}"
    );
    assert!(
        body.contains("# TYPE hbbs_register_total counter"),
        "missing register_total TYPE: {body}"
    );
    // counter 值至少应包含 hbbs_register_total{kind="peer"} 一行
    assert!(
        body.contains("hbbs_register_total{kind=\"peer\"}"),
        "missing labeled metric: {body}"
    );
}
