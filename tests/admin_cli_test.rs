// CE-M0-7 集成测试:管理 CLI UDS + token。
//
// 这些测试通过实现一个 Mock `AdminCmd` 并直接调用 `admin_cli::spawn_listener`,
// 不依赖 hbbs / hbbr 完整启动栈。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use hbbs::admin_cli::{self, AdminCmd, AdminConfig};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use hbb_common::tokio::{
    self,
    io::{AsyncReadExt, AsyncWriteExt},
};

struct CountingHandler {
    invocations: AtomicUsize,
    last_cmd: tokio::sync::Mutex<String>,
}

impl CountingHandler {
    fn new() -> Self {
        Self {
            invocations: AtomicUsize::new(0),
            last_cmd: tokio::sync::Mutex::new(String::new()),
        }
    }
}

#[async_trait::async_trait]
impl AdminCmd for CountingHandler {
    async fn run(&self, cmd: &str) -> String {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        *self.last_cmd.lock().await = cmd.to_string();
        format!("OK:{}\n", cmd.trim())
    }
}

fn tmp_paths(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let pid = std::process::id();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    // 注意:macOS 上 UDS 路径受 SUN_LEN (104) 限制,所以这里用 /tmp 而不是
    // env::temp_dir() (后者在 macOS 上是 /var/folders/.../T 这种长路径)。
    let root = std::path::PathBuf::from("/tmp");
    let base = root.join(format!("rd-ce-m07-{}-{}-{:x}", tag, pid, now as u64));
    std::fs::create_dir_all(&base).unwrap();
    (base.join("a.sock"), base.join("a.token"))
}

async fn read_token(token_path: &std::path::Path) -> String {
    // 等 spawn_listener 完成 token 文件写入(异步)。
    for _ in 0..100 {
        if let Ok(s) = std::fs::read_to_string(token_path) {
            if let Some(line) = s.lines().next() {
                if !line.is_empty() {
                    return line.to_string();
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("token file not written: {:?}", token_path);
}

async fn wait_socket(path: &std::path::Path) {
    for _ in 0..100 {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("socket not created: {:?}", path);
}

async fn send_recv(socket: &std::path::Path, payload: &[u8]) -> Vec<u8> {
    let mut s = tokio::net::UnixStream::connect(socket).await.unwrap();
    s.write_all(payload).await.unwrap();
    s.shutdown().await.ok();
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_millis(2000), s.read_to_end(&mut buf)).await;
    buf
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn uds_auth_happy_path() {
    let (sock, token) = tmp_paths("happy");
    let handler = Arc::new(CountingHandler::new());
    admin_cli::spawn_listener(
        "test-hbbs",
        AdminConfig {
            socket_path: Some(sock.clone()),
            tcp_addr: None,
            token_file: token.clone(),
            disabled: false,
        },
        handler.clone(),
    );
    let t = read_token(&token).await;
    wait_socket(&sock).await;

    // 验证 socket 权限
    let meta = std::fs::metadata(&sock).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o660, "socket mode");

    let payload = format!("{} hello-cmd\n", t);
    let resp = send_recv(&sock, payload.as_bytes()).await;
    assert_eq!(resp, b"OK:hello-cmd\n");
    assert_eq!(handler.invocations.load(Ordering::SeqCst), 1);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn uds_reject_missing_token() {
    let (sock, token) = tmp_paths("missing");
    let handler = Arc::new(CountingHandler::new());
    admin_cli::spawn_listener(
        "test-hbbs",
        AdminConfig {
            socket_path: Some(sock.clone()),
            tcp_addr: None,
            token_file: token.clone(),
            disabled: false,
        },
        handler.clone(),
    );
    let _ = read_token(&token).await;
    wait_socket(&sock).await;

    let resp = send_recv(&sock, b"h\n").await;
    assert_eq!(resp, b"ERR unauthorized\n");
    assert_eq!(handler.invocations.load(Ordering::SeqCst), 0);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn uds_reject_wrong_token() {
    let (sock, token) = tmp_paths("wrong");
    let handler = Arc::new(CountingHandler::new());
    admin_cli::spawn_listener(
        "test-hbbs",
        AdminConfig {
            socket_path: Some(sock.clone()),
            tcp_addr: None,
            token_file: token.clone(),
            disabled: false,
        },
        handler.clone(),
    );
    let _ = read_token(&token).await;
    wait_socket(&sock).await;

    let resp = send_recv(&sock, b"deadbeef h\n").await;
    assert_eq!(resp, b"ERR unauthorized\n");
    assert_eq!(handler.invocations.load(Ordering::SeqCst), 0);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn uds_reject_oversized() {
    let (sock, token) = tmp_paths("oversize");
    let handler = Arc::new(CountingHandler::new());
    admin_cli::spawn_listener(
        "test-hbbs",
        AdminConfig {
            socket_path: Some(sock.clone()),
            tcp_addr: None,
            token_file: token.clone(),
            disabled: false,
        },
        handler.clone(),
    );
    let _ = read_token(&token).await;
    wait_socket(&sock).await;

    // 2048 字节零数据;handler 必不被调用,服务端可能回 ERR too-large 或直接关闭。
    let big = vec![b'A'; 2048];
    let resp = send_recv(&sock, &big).await;
    // 允许两种行为:返回 ERR too-large 或空(关闭)。
    if !resp.is_empty() {
        assert!(
            resp.starts_with(b"ERR"),
            "got non-error response: {:?}",
            String::from_utf8_lossy(&resp)
        );
    }
    assert_eq!(handler.invocations.load(Ordering::SeqCst), 0);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn token_file_perms() {
    let (sock, token) = tmp_paths("perms");
    let handler = Arc::new(CountingHandler::new());
    admin_cli::spawn_listener(
        "test-hbbs",
        AdminConfig {
            socket_path: Some(sock.clone()),
            tcp_addr: None,
            token_file: token.clone(),
            disabled: false,
        },
        handler.clone(),
    );
    let _ = read_token(&token).await;

    let meta = std::fs::metadata(&token).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o640, "token file mode");
    let parent_meta = std::fs::metadata(token.parent().unwrap()).unwrap();
    // 父目录是 tmp_paths 创建的,默认 0o755 / 0o700;只要不比 0o755 宽即可。
    let mode = parent_meta.permissions().mode() & 0o777;
    assert!(mode <= 0o755, "parent mode too wide: {:o}", mode);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn disabled_flag_no_socket() {
    let (sock, token) = tmp_paths("disabled");
    let handler = Arc::new(CountingHandler::new());
    admin_cli::spawn_listener(
        "test-hbbs",
        AdminConfig {
            socket_path: Some(sock.clone()),
            tcp_addr: None,
            token_file: token.clone(),
            disabled: true,
        },
        handler.clone(),
    );
    // 等 200ms 给后台 task 机会跑(预期不会做任何事)。
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!sock.exists(), "socket should not be created when disabled");
    assert!(!token.exists(), "token should not be written when disabled");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn tcp_loopback_only() {
    // 非 loopback 应直接 bind 失败,不写 token。
    let (_sock, token) = tmp_paths("tcp_loopback");
    let handler = Arc::new(CountingHandler::new());
    admin_cli::spawn_listener(
        "test-hbbs",
        AdminConfig {
            socket_path: None,
            tcp_addr: Some("0.0.0.0:0".to_string()),
            token_file: token.clone(),
            disabled: false,
        },
        handler.clone(),
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !token.exists(),
        "token should not be written when bind rejected"
    );
}
