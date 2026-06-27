// CE-M0-3: Prometheus metrics 独立端口
//
// 本模块为 hbbs / hbbr 提供独立的 Prometheus `/metrics` HTTP 暴露端点。
// - 通过 `metrics` facade + `metrics-exporter-prometheus` 实现,默认不启用,
//   只有当 CLI 指定 `--metrics-bind <ip>:<port>` 时才会安装 recorder 并监听端口。
// - 不占用 21114(rustdesk-api)/21115-21119(rendezvous/relay) 等既有端口,
//   建议默认 21120 (hbbs) / 21121 (hbbr),loopback 绑定。
// - 所有 label value 严格使用枚举常量,禁止使用 peer_id / IP / uuid 等高基数字段。
//
// 详见 docs/ai-tasks/CE-M0-3.md。

// hbbr binary 通过 `mod metrics;` 引入本文件,但只用 hbbr 相关函数;
// hbbs binary 同样会有未使用的 hbbr 函数。统一 allow 掉以保持模块单文件。
#![allow(dead_code)]

use std::net::SocketAddr;

use hbb_common::{bail, log, ResultType};
use metrics::{
    counter, decrement_gauge, describe_counter, describe_gauge, describe_histogram, gauge,
    histogram, increment_gauge, Unit,
};
use metrics_exporter_prometheus::PrometheusBuilder;

/// 安装 Prometheus recorder 并启动 HTTP listener。
///
/// * `bind`  - loopback `SocketAddr`,例如 `127.0.0.1:21120`。
/// * `role`  - "hbbs" 或 "hbbr",用于 `*_build_info` 的 `role` label。
pub fn install_recorder(bind: SocketAddr, role: &'static str) -> ResultType<()> {
    let builder = PrometheusBuilder::new().with_http_listener(bind);
    if let Err(e) = builder.install() {
        bail!("--metrics-bind {} install failed: {}", bind, e);
    }
    register_descriptions(role);
    // build_info gauge: 始终为 1,用于看板按 role / version 分组。
    let version = env!("CARGO_PKG_VERSION");
    match role {
        "hbbs" => {
            gauge!("hbbs_build_info", 1.0, "role" => role, "version" => version);
        }
        "hbbr" => {
            gauge!("hbbr_build_info", 1.0, "role" => role, "version" => version);
        }
        _ => {
            log::warn!("metrics::install_recorder: unknown role {role}");
        }
    }
    log::info!("metrics: prometheus listener installed on {bind} (role={role})");
    Ok(())
}

/// 一次性注册所有 metric 的描述/单位/类型,保证即使没有观测值,scrape 时也能拿到
/// `# TYPE`/`# HELP` 行,避免 Grafana 出现 unknown metric。
fn register_descriptions(role: &str) {
    match role {
        "hbbs" => {
            describe_gauge!("hbbs_build_info", "hbbs build info (always 1)");
            describe_gauge!(
                "hbbs_peers_online",
                "current online peers (last_reg within REG_TIMEOUT)"
            );
            describe_gauge!("hbbs_peermap_entries", "PeerMap in-memory entry count");
            describe_gauge!("hbbs_ws_connections_active", "active websocket connections");
            describe_counter!("hbbs_register_total", "register_peer / register_pk events");
            describe_counter!("hbbs_register_reject_total", "register_pk reject events");
            describe_counter!(
                "hbbs_punch_hole_total",
                "punch_hole_request events by transport"
            );
            describe_counter!(
                "hbbs_punch_hole_result_total",
                "punch_hole result branch counters"
            );
            describe_counter!(
                "hbbs_relay_assign_total",
                "relay server assigned to a request"
            );
            describe_counter!("hbbs_ip_blocked_total", "register rejected by ip blocker");
            describe_histogram!(
                "hbbs_api_access_check_seconds",
                Unit::Seconds,
                "API/RBAC access check latency (filled by CE-M2)"
            );
        }
        "hbbr" => {
            describe_gauge!("hbbr_build_info", "hbbr build info (always 1)");
            describe_gauge!("hbbr_sessions_active", "currently relaying sessions");
            describe_gauge!(
                "hbbr_pair_pending",
                "first-side requests waiting for the second peer"
            );
            describe_counter!(
                "hbbr_pair_timeout_total",
                "pair waited 30s without partner showing up"
            );
            describe_counter!("hbbr_bytes_total", "relay bytes (bits) by direction");
            describe_counter!("hbbr_limiter_consume_total", "limiter consume events");
            describe_counter!("hbbr_downgrade_total", "relay flows downgraded");
            describe_counter!("hbbr_blocked_total", "blocked by blocklist");
            describe_counter!("hbbr_relay_close_total", "relay close reason");
        }
        _ => {}
    }
}

// ---------- hbbs 便捷函数 ----------

#[inline]
pub fn inc_register(kind: &'static str) {
    counter!("hbbs_register_total", 1, "kind" => kind);
}

#[inline]
pub fn inc_register_reject(reason: &'static str) {
    counter!("hbbs_register_reject_total", 1, "reason" => reason);
}

#[inline]
pub fn inc_punch_hole(transport: &'static str) {
    counter!("hbbs_punch_hole_total", 1, "transport" => transport);
}

#[inline]
pub fn inc_punch_hole_result(result: &'static str) {
    counter!("hbbs_punch_hole_result_total", 1, "result" => result);
}

#[inline]
pub fn inc_relay_assign() {
    counter!("hbbs_relay_assign_total", 1);
}

#[inline]
pub fn set_peers_online(v: i64) {
    gauge!("hbbs_peers_online", v as f64);
}

#[inline]
pub fn set_peermap_entries(v: i64) {
    gauge!("hbbs_peermap_entries", v as f64);
}

#[inline]
pub fn observe_access_check(decision: &'static str, seconds: f64) {
    histogram!("hbbs_api_access_check_seconds", seconds, "decision" => decision);
}

#[inline]
pub fn inc_ws_active(delta: i64) {
    if delta >= 0 {
        increment_gauge!("hbbs_ws_connections_active", delta as f64);
    } else {
        decrement_gauge!("hbbs_ws_connections_active", (-delta) as f64);
    }
}

#[inline]
pub fn inc_ip_blocked() {
    counter!("hbbs_ip_blocked_total", 1);
}

// ---------- hbbr 便捷函数 ----------

#[inline]
pub fn inc_session_active(delta: i64) {
    if delta >= 0 {
        increment_gauge!("hbbr_sessions_active", delta as f64);
    } else {
        decrement_gauge!("hbbr_sessions_active", (-delta) as f64);
    }
}

#[inline]
pub fn inc_pair_pending(delta: i64) {
    if delta >= 0 {
        increment_gauge!("hbbr_pair_pending", delta as f64);
    } else {
        decrement_gauge!("hbbr_pair_pending", (-delta) as f64);
    }
}

#[inline]
pub fn inc_pair_timeout() {
    counter!("hbbr_pair_timeout_total", 1);
}

#[inline]
pub fn add_bytes(dir: &'static str, n: u64) {
    counter!("hbbr_bytes_total", n, "dir" => dir);
}

#[inline]
pub fn inc_limiter_consume(class: &'static str) {
    counter!("hbbr_limiter_consume_total", 1, "class" => class);
}

#[inline]
pub fn inc_downgrade() {
    counter!("hbbr_downgrade_total", 1);
}

#[inline]
pub fn inc_blocked(kind: &'static str) {
    counter!("hbbr_blocked_total", 1, "kind" => kind);
}

#[inline]
pub fn inc_relay_close(reason: &'static str) {
    counter!("hbbr_relay_close_total", 1, "reason" => reason);
}
