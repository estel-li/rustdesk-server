// CE-M1-8 集成测试:通过 WebSocket (ws_port = port + 2) 走 RegisterPk / RegisterPeer 流程。
//
// 这些用例不依赖任何真实 RustDesk 客户端;直接以 tokio-tungstenite 发送二进制
// `RendezvousMessage` 帧,断言服务端返回与共享 helper 内部状态的预期。
//
// 测试设计:
//   * 用 `Once` + 后台 std::thread 启动一次 `RendezvousServer::start`,
//     `start` 内部 `#[tokio::main]` 会建立自己的 runtime,与 `#[tokio::test]`
//     的 runtime 互不干扰。
//   * 每个测试通过独立 `id` 隔离 peer 状态,共享同一台 server 进程实例;
//     必要时(rate limit 用例)走独立 ID 避免污染其他用例。
//   * `TEST_HBBS=no`、`ADMIN-DISABLE=Y` 关闭 hbbs 自检 / 管理 CLI,
//     `DB_URL` 指向临时 sqlite 文件。

use std::net::{TcpListener, UdpSocket};
use std::sync::Once;
use std::time::Duration;

use hbb_common::futures_util::{SinkExt, StreamExt};
use hbb_common::protobuf::Message as _;
use hbb_common::rendezvous_proto::{
    register_pk_response, rendezvous_message, RegisterPeer, RegisterPk, RendezvousMessage,
};
use hbb_common::tokio::{self, time::sleep};
use tokio_tungstenite::tungstenite;

static INIT: Once = Once::new();
static mut WS_PORT: u16 = 0;

/// 尝试在一个端口上同时分配 TCP / UDP,验证 hbbs 用到的端口集合都空闲。
fn try_reserve(port: u16) -> bool {
    let nat_port = port.saturating_sub(1);
    let ws_port = port.checked_add(2);
    let ws_port = match ws_port {
        Some(p) => p,
        None => return false,
    };
    let t1 = TcpListener::bind(("127.0.0.1", port)).ok();
    let t2 = TcpListener::bind(("127.0.0.1", nat_port)).ok();
    let t3 = TcpListener::bind(("127.0.0.1", ws_port)).ok();
    let u1 = UdpSocket::bind(("127.0.0.1", port)).ok();
    let ok = t1.is_some() && t2.is_some() && t3.is_some() && u1.is_some();
    drop((t1, t2, t3, u1));
    ok
}

fn pick_base_port() -> u16 {
    // 高位区间避免与系统服务冲突;以 pid 取模做粗扰动。
    let pid = std::process::id() as u16;
    let mut p: u16 = 30000u16.wrapping_add(pid % 5000);
    if p < 1024 {
        p = 30000;
    }
    for _ in 0..200 {
        if try_reserve(p) {
            return p;
        }
        p = p.wrapping_add(7);
        if p < 1024 {
            p = 30000;
        }
    }
    panic!("could not find a free base port for hbbs");
}

fn ensure_server() -> u16 {
    INIT.call_once(|| {
        let port = pick_base_port();
        // DB 文件用唯一名,避免污染仓库根目录。
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut db_path = std::env::temp_dir();
        db_path.push(format!("rustdesk_ce_m1_8_{}_{}.sqlite3", port, nonce));
        std::env::set_var("DB_URL", db_path.to_string_lossy().to_string());
        // 跳过 hbbs 自检与管理 CLI。
        std::env::set_var("TEST_HBBS", "no");
        std::env::set_var("ADMIN-DISABLE", "Y");

        std::thread::spawn(move || {
            // start() 内部走 #[tokio::main(multi_thread)],自带 runtime。
            // 在独立 OS 线程里跑,这里阻塞直到进程结束,测试不主动停止。
            let _ = hbbs::RendezvousServer::start(port as i32, 0, "_", 0);
        });

        // 等待 ws_port 监听就绪。
        let ws_port = port + 2;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if std::net::TcpStream::connect_timeout(
                &format!("127.0.0.1:{ws_port}").parse().unwrap(),
                Duration::from_millis(200),
            )
            .is_ok()
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        unsafe {
            WS_PORT = ws_port;
        }
    });
    unsafe { WS_PORT }
}

/// 通过 WS 建立连接,发送一个 `RendezvousMessage`,等待对端的第一帧 Binary。
async fn ws_round_trip(ws_port: u16, msg: &RendezvousMessage) -> RendezvousMessage {
    ws_round_trip_n(ws_port, msg, 1).await.pop().unwrap()
}

/// 同上,但等待 `n` 帧后再断开。
async fn ws_round_trip_n(
    ws_port: u16,
    msg: &RendezvousMessage,
    n: usize,
) -> Vec<RendezvousMessage> {
    let url = format!("ws://127.0.0.1:{ws_port}");
    let (mut ws, _resp) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("ws connect");
    let bytes = msg.write_to_bytes().expect("encode");
    ws.send(tungstenite::Message::Binary(bytes))
        .await
        .expect("send");

    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let frame = tokio::time::timeout(Duration::from_secs(3), ws.next())
            .await
            .expect("recv timeout")
            .expect("ws closed")
            .expect("ws err");
        if let tungstenite::Message::Binary(b) = frame {
            let parsed = RendezvousMessage::parse_from_bytes(&b).expect("parse");
            out.push(parsed);
        }
    }
    let _ = ws.close(None).await;
    out
}

fn rk_msg(id: &str, uuid: &[u8], pk: &[u8]) -> RendezvousMessage {
    let mut m = RendezvousMessage::new();
    m.set_register_pk(RegisterPk {
        id: id.to_owned(),
        uuid: uuid.to_vec().into(),
        pk: pk.to_vec().into(),
        ..Default::default()
    });
    m
}

fn rp_msg(id: &str, serial: i32) -> RendezvousMessage {
    let mut m = RendezvousMessage::new();
    m.set_register_peer(RegisterPeer {
        id: id.to_owned(),
        serial,
        ..Default::default()
    });
    m
}

#[tokio::test(flavor = "multi_thread")]
async fn ws_register_pk_happy_path() {
    let ws_port = ensure_server();
    let id = "ce_m18_a";
    let uuid = b"uuid-a-0001";
    let pk = b"pk-a-0001";
    let resp = ws_round_trip(ws_port, &rk_msg(id, uuid, pk)).await;
    match resp.union {
        Some(rendezvous_message::Union::RegisterPkResponse(r)) => {
            assert_eq!(
                r.result.enum_value(),
                Ok(register_pk_response::Result::OK),
                "expected OK got {:?}",
                r.result
            );
        }
        other => panic!("unexpected response: {:?}", other),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ws_register_pk_uuid_mismatch() {
    let ws_port = ensure_server();
    let id = "ce_m18_b";
    // 首次注册 OK。
    let r1 = ws_round_trip(ws_port, &rk_msg(id, b"uuid-b-1", b"pk-b-1")).await;
    if let Some(rendezvous_message::Union::RegisterPkResponse(r)) = r1.union {
        assert_eq!(r.result.enum_value(), Ok(register_pk_response::Result::OK));
    } else {
        panic!("first reg failed");
    }
    // 让 reg_pk 节流 (>2 in 6s) 不挡道:等 7 秒。
    sleep(Duration::from_secs(7)).await;
    // 同 id 不同 uuid -> UUID_MISMATCH。
    let r2 = ws_round_trip(ws_port, &rk_msg(id, b"uuid-b-2", b"pk-b-1")).await;
    if let Some(rendezvous_message::Union::RegisterPkResponse(r)) = r2.union {
        assert_eq!(
            r.result.enum_value(),
            Ok(register_pk_response::Result::UUID_MISMATCH),
            "expected UUID_MISMATCH got {:?}",
            r.result
        );
    } else {
        panic!("second resp shape unexpected");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ws_register_pk_rate_limited() {
    let ws_port = ensure_server();
    let id = "ce_m18_c";
    let uuid = b"uuid-c-1";
    let pk = b"pk-c-1";
    let mut results = Vec::new();
    for _ in 0..4 {
        let r = ws_round_trip(ws_port, &rk_msg(id, uuid, pk)).await;
        if let Some(rendezvous_message::Union::RegisterPkResponse(rr)) = r.union {
            results.push(
                rr.result
                    .enum_value()
                    .unwrap_or(register_pk_response::Result::OK),
            );
        }
    }
    // 前 3 次允许成功;第 4 次必为 TOO_FREQUENT(reg_pk.0 > 2 触发)。
    assert!(
        results.contains(&register_pk_response::Result::TOO_FREQUENT),
        "expected at least one TOO_FREQUENT, got {:?}",
        results
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ws_register_peer_returns_request_pk_when_new() {
    let ws_port = ensure_server();
    let id = "ce_m18_d";
    let resp = ws_round_trip(ws_port, &rp_msg(id, 0)).await;
    match resp.union {
        Some(rendezvous_message::Union::RegisterPeerResponse(r)) => {
            // 新 id 内存中没有 pk,必须要求客户端补 RegisterPk。
            assert!(r.request_pk, "expected request_pk=true for fresh id");
        }
        other => panic!("unexpected response: {:?}", other),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ws_register_peer_serial_not_triggered_when_equal() {
    // 服务端 serial=0(start 传入),客户端 serial=0,不应附带 ConfigUpdate。
    let ws_port = ensure_server();
    let id = "ce_m18_e";
    let mut got = Vec::new();
    // 只读一帧;若服务端真的发了 ConfigUpdate,会读不到/超时(用更短超时单独跑)。
    let url = format!("ws://127.0.0.1:{ws_port}");
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.expect("ws");
    let bytes = rp_msg(id, 0).write_to_bytes().unwrap();
    ws.send(tungstenite::Message::Binary(bytes)).await.unwrap();
    // 第一帧:RegisterPeerResponse
    let frame = tokio::time::timeout(Duration::from_secs(3), ws.next())
        .await
        .expect("timeout")
        .expect("closed")
        .expect("err");
    if let tungstenite::Message::Binary(b) = frame {
        got.push(RendezvousMessage::parse_from_bytes(&b).unwrap());
    }
    // 第二帧:不应到达。用 500ms 短超时验证。
    let second = tokio::time::timeout(Duration::from_millis(500), ws.next()).await;
    assert!(second.is_err(), "did not expect a second frame");
    let _ = ws.close(None).await;
    assert_eq!(got.len(), 1);
    assert!(matches!(
        got[0].union,
        Some(rendezvous_message::Union::RegisterPeerResponse(_))
    ));
}
