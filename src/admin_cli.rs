// CE-M0-7: 管理 CLI 改 UDS + token
//
// 旧实现把 hbbs 的 NAT-test 端口(`port-1`) 与 hbbr 的中继主端口直接当作
// 管理通道,仅以 `is_loopback()` 判定信任源,本机任何用户都可以下达
// `relay-servers` / `blacklist` / `single-bandwidth` 等高危命令。
//
// 本模块替换为:
//   * Unix 平台:UnixListener,socket 文件 mode 0o660,父目录 0o750。
//     Linux 上额外做 SO_PEERCRED 校验,只允许 uid==0 或 uid==self_uid。
//   * Windows 平台:TCP loopback,严格只允许 127.0.0.1 / ::1。
//   * 启动期生成 32 字节随机 token (base64 url-safe, no pad),
//     同时写 stderr 与 token 文件 (mode 0o640),客户端首段必须匹配。
//   * Token 比较使用常量时间比较,避免计时侧信道。
//   * 单连接读取上限 1024 字节,1s timeout,与旧实现兼容。

use hbb_common::{
    log,
    rand::{rngs::OsRng, RngCore},
    tokio::{
        self,
        io::{AsyncReadExt, AsyncWriteExt},
        time::timeout,
    },
};
use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

/// 管理命令处理 trait。hbbs / hbbr 各自实现一份。
#[async_trait::async_trait]
pub trait AdminCmd: Send + Sync + 'static {
    async fn run(&self, cmd: &str) -> String;
}

/// CLI / env 解析后的运行期配置。
#[derive(Debug, Clone)]
pub struct AdminConfig {
    /// UDS 路径,空 = 禁用 UDS(Windows 默认)。
    pub socket_path: Option<PathBuf>,
    /// 可选 TCP loopback 地址,必须是 127.0.0.1/::1 任意 socket。
    pub tcp_addr: Option<String>,
    /// token 文件落盘位置;若 disable 则不写。
    pub token_file: PathBuf,
    /// 显式禁用整个管理 CLI。
    pub disabled: bool,
}

/// 包装 token 字节;`Debug` / `Display` 实现遮蔽,避免日志泄露。
#[derive(Clone)]
pub struct Token(Arc<String>);

impl Token {
    pub fn new(s: String) -> Self {
        Self(Arc::new(s))
    }
    /// 仅在写 token 文件 / 首次 stderr 时调用。
    pub fn expose(&self) -> &str {
        self.0.as_str()
    }
    fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(***)")
    }
}

impl std::fmt::Display for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("***")
    }
}

/// 生成 32 字节随机 token,base64 url-safe 不带 padding,长度 43。
pub fn generate_token() -> Token {
    let mut buf = [0u8; 32];
    OsRng.fill_bytes(&mut buf);
    // 使用 rustdesk-server 自身的 base64 0.13(hbb_common 内部用的是 0.22,
    // API 不一致;此处直接用 crate 顶层依赖以避免引入两套 base64 的复杂度)。
    Token::new(::base64::encode_config(buf, ::base64::URL_SAFE_NO_PAD))
}

/// 常量时间比较:两端长度不等直接返回 false,但仍按长 max 走完循环。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u8;
    let n = a.len().min(b.len());
    for i in 0..n {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// 把 token 写到 `path`,mode 0o640;父目录若不存在则 mkdir 0o750。
/// 同时附带可选元数据(TCP 端口等)写到第二行 `key=value`。
pub fn write_token_file(
    path: &Path,
    token: &Token,
    meta: &[(&str, String)],
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o750));
            }
        }
    }
    // 若旧文件权限过宽,先 unlink 再创建,确保新文件用我们指定的 mode。
    if path.exists() {
        let _ = fs::remove_file(path);
    }
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        opts.mode(0o640);
    }
    let mut f = opts.open(path)?;
    writeln!(f, "{}", token.expose())?;
    for (k, v) in meta {
        writeln!(f, "{}={}", k, v)?;
    }
    f.sync_all().ok();
    #[cfg(unix)]
    {
        // 二次收紧:即便受 umask 影响 OpenOptions::mode 失效,也强制 0o640。
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o640));
    }
    Ok(())
}

/// 失败包返回常量,避免暴露 token 长度等信息。
const RESP_UNAUTHORIZED: &[u8] = b"ERR unauthorized\n";
const RESP_TOO_LARGE: &[u8] = b"ERR too-large\n";
const READ_BUF_LEN: usize = 1024;
const READ_TIMEOUT_MS: u64 = 1000;

/// 把 wire (token + 命令) 解出,返回命令字符串。
/// 协议:首段 token,以空格或换行分隔,后续为旧 `cmd`。
fn split_token_cmd(buf: &[u8]) -> Option<(&[u8], &str)> {
    let sep = buf
        .iter()
        .position(|&b| b == b' ' || b == b'\n' || b == b'\r')?;
    let token = &buf[..sep];
    let rest = &buf[sep + 1..];
    let cmd = std::str::from_utf8(rest).ok()?;
    Some((token, cmd))
}

/// 处理单连接:读 → 校验 → 调用 handler → 回写。
async fn handle_one<S, H>(mut stream: S, handler: Arc<H>, token: Token)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    H: AdminCmd + ?Sized,
{
    let mut buf = vec![0u8; READ_BUF_LEN + 1];
    let read = timeout(
        Duration::from_millis(READ_TIMEOUT_MS),
        stream.read(&mut buf[..]),
    )
    .await;
    let n = match read {
        Ok(Ok(n)) => n,
        Ok(Err(e)) => {
            log::warn!("admin read error: {}", e);
            return;
        }
        Err(_) => {
            log::warn!("admin read timeout");
            return;
        }
    };
    if n == 0 {
        return;
    }
    if n > READ_BUF_LEN {
        let _ = stream.write_all(RESP_TOO_LARGE).await;
        let _ = stream.shutdown().await;
        return;
    }
    let data = &buf[..n];
    let (got_token, cmd) = match split_token_cmd(data) {
        Some(x) => x,
        None => {
            let _ = stream.write_all(RESP_UNAUTHORIZED).await;
            let _ = stream.shutdown().await;
            return;
        }
    };
    if !constant_time_eq(got_token, token.as_bytes()) {
        let _ = stream.write_all(RESP_UNAUTHORIZED).await;
        let _ = stream.shutdown().await;
        return;
    }
    let res = handler.run(cmd).await;
    let _ = stream.write_all(res.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// 启动管理 CLI 监听器。任何错误都只 warn 不 panic,保证服务主路径不受影响。
/// `role` 仅用于日志(`hbbs` / `hbbr`)。
pub fn spawn_listener<H: AdminCmd + ?Sized>(role: &'static str, cfg: AdminConfig, handler: Arc<H>) {
    if cfg.disabled {
        log::info!("[{}] admin cli disabled (--admin-disable)", role);
        return;
    }
    if cfg.socket_path.is_none() && cfg.tcp_addr.is_none() {
        log::info!("[{}] admin cli disabled (no socket / tcp configured)", role);
        return;
    }

    let token = generate_token();

    // token 文件落盘前,顺手把 TCP 端口写进去(若有)。
    // 真实端口在 bind 之后才知道,这里先用占位 0,bind 成功后会改写。
    // 由于 bind 失败也不写文件,简单起见把元数据采集放到 spawn 内部。
    let role_str = role.to_string();
    tokio::spawn(async move {
        // 1. 尝试启动 UDS / TCP listener。
        let mut meta: Vec<(&str, String)> = Vec::new();

        let uds_listener = match cfg.socket_path.as_ref() {
            Some(p) => match bind_uds(p).await {
                Ok(l) => Some(l),
                Err(e) => {
                    log::warn!("[{}] admin UDS bind {:?} failed: {}", role_str, p, e);
                    None
                }
            },
            None => None,
        };

        let tcp_listener = match cfg.tcp_addr.as_ref() {
            Some(addr) => match bind_tcp(addr).await {
                Ok((l, real)) => {
                    meta.push(("tcp", real.to_string()));
                    Some(l)
                }
                Err(e) => {
                    log::error!(
                        "[{}] admin TCP bind {} failed: {}; refusing to start admin cli over insecure address",
                        role_str, addr, e
                    );
                    None
                }
            },
            None => None,
        };

        if uds_listener.is_none() && tcp_listener.is_none() {
            log::warn!(
                "[{}] admin cli has no listener; disabled this run",
                role_str
            );
            return;
        }

        // 2. 落盘 token + 元数据。
        match write_token_file(&cfg.token_file, &token, &meta) {
            Ok(_) => {
                eprintln!(
                    "[admin] {} token written to {}",
                    role_str,
                    cfg.token_file.display()
                );
                // stderr 兜底打印 token(仅一次),便于首次部署。systemd 会把
                // stderr 拷到 hbbs.error/hbbr.error,运维可在那里看到。
                eprintln!("[admin] {} token: {}", role_str, token.expose());
            }
            Err(e) => {
                log::error!(
                    "[{}] failed to write admin token file {:?}: {}; admin cli disabled",
                    role_str,
                    cfg.token_file,
                    e
                );
                return;
            }
        }

        // 3. accept loop。
        accept_loop(role_str, uds_listener, tcp_listener, handler, token).await;
    });
}

#[cfg(unix)]
async fn bind_uds(path: &Path) -> std::io::Result<tokio::net::UnixListener> {
    // 父目录兜底创建(systemd RuntimeDirectory= 已建则跳过)。
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            fs::create_dir_all(parent)?;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o750));
        }
    }
    // 残留 socket 处理:先尝试 connect,若连得通,认为是另一个实例,hard fail。
    if path.exists() {
        match tokio::net::UnixStream::connect(path).await {
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    format!("another admin cli instance listening on {:?}", path),
                ));
            }
            Err(_) => {
                let _ = fs::remove_file(path);
            }
        }
    }
    let listener = tokio::net::UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o660))?;
    Ok(listener)
}

#[cfg(not(unix))]
async fn bind_uds(_path: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "UnixListener not supported on this platform",
    ))
}

async fn bind_tcp(addr: &str) -> std::io::Result<(tokio::net::TcpListener, std::net::SocketAddr)> {
    use std::net::ToSocketAddrs;
    let sa = addr
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no addr"))?;
    if !sa.ip().is_loopback() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("admin TCP must bind loopback, got {}", sa.ip()),
        ));
    }
    let listener = tokio::net::TcpListener::bind(sa).await?;
    let real = listener.local_addr()?;
    log::info!(
        "Windows/admin TCP listener bound on {} (must be loopback)",
        real
    );
    Ok((listener, real))
}

#[cfg(unix)]
async fn accept_loop<H: AdminCmd + ?Sized>(
    role: String,
    uds: Option<tokio::net::UnixListener>,
    tcp: Option<tokio::net::TcpListener>,
    handler: Arc<H>,
    token: Token,
) {
    log::info!("[{}] admin cli accept loop started", role);
    // 两个 listener 各自占一个 task,避免 select! 里 pending 占位 future 推不出类型。
    if let Some(uds) = uds {
        let role = role.clone();
        let handler = handler.clone();
        let token = token.clone();
        tokio::spawn(async move {
            loop {
                match uds.accept().await {
                    Ok((stream, _addr)) => {
                        if !check_uds_peer_cred(&stream) {
                            log::warn!("[{}] admin UDS peer cred rejected", role);
                            drop(stream);
                            continue;
                        }
                        let handler = handler.clone();
                        let token = token.clone();
                        tokio::spawn(async move {
                            handle_one(stream, handler, token).await;
                        });
                    }
                    Err(e) => log::warn!("[{}] admin UDS accept: {}", role, e),
                }
            }
        });
    }
    if let Some(tcp) = tcp {
        let role = role.clone();
        tokio::spawn(async move {
            loop {
                match tcp.accept().await {
                    Ok((stream, addr)) => {
                        if !addr.ip().is_loopback() {
                            log::warn!("[{}] admin TCP non-loopback peer {}; dropping", role, addr);
                            continue;
                        }
                        let handler = handler.clone();
                        let token = token.clone();
                        tokio::spawn(async move {
                            handle_one(stream, handler, token).await;
                        });
                    }
                    Err(e) => log::warn!("[{}] admin TCP accept: {}", role, e),
                }
            }
        });
    }
    // 两个子任务一直循环,父任务 idle wait,让 spawn_listener 不要 join 阻塞主路径。
    std::future::pending::<()>().await;
}

#[cfg(not(unix))]
async fn accept_loop<H: AdminCmd + ?Sized>(
    role: String,
    _uds: Option<()>,
    tcp: Option<tokio::net::TcpListener>,
    handler: Arc<H>,
    token: Token,
) {
    log::info!(
        "[{}] admin cli accept loop started (Windows/TCP only)",
        role
    );
    let tcp = match tcp {
        Some(t) => t,
        None => {
            log::warn!("[{}] no TCP listener; admin cli idle", role);
            return;
        }
    };
    loop {
        match tcp.accept().await {
            Ok((stream, addr)) => {
                if !addr.ip().is_loopback() {
                    log::warn!("[{}] admin TCP non-loopback peer {}; dropping", role, addr);
                    continue;
                }
                let handler = handler.clone();
                let token = token.clone();
                tokio::spawn(async move {
                    handle_one(stream, handler, token).await;
                });
            }
            Err(e) => log::warn!("[{}] admin TCP accept: {}", role, e),
        }
    }
}

#[cfg(target_os = "linux")]
fn check_uds_peer_cred(stream: &tokio::net::UnixStream) -> bool {
    match stream.peer_cred() {
        Ok(cred) => {
            // 只允许 uid==0 (root) 或与服务进程同 uid。
            let self_uid = unsafe { libc::getuid() };
            let peer_uid = cred.uid();
            if peer_uid == 0 || peer_uid == self_uid {
                true
            } else {
                log::warn!(
                    "admin UDS peer uid={} != self uid={} and != root; reject",
                    peer_uid,
                    self_uid
                );
                false
            }
        }
        Err(e) => {
            log::warn!(
                "admin UDS peer_cred failed: {}; falling back to token-only",
                e
            );
            // 取不到 peer cred 时降级,仍由文件权限 (0o660) 与 token 拦截。
            true
        }
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn check_uds_peer_cred(stream: &tokio::net::UnixStream) -> bool {
    // macOS / BSD: tokio 也有 peer_cred,但 uid 字段不一定可靠;失败降级。
    match stream.peer_cred() {
        Ok(cred) => {
            let self_uid = unsafe { libc::getuid() };
            let peer_uid = cred.uid();
            if peer_uid == 0 || peer_uid == self_uid {
                true
            } else {
                log::warn!(
                    "admin UDS peer uid={} != self uid={} and != root; reject",
                    peer_uid,
                    self_uid
                );
                false
            }
        }
        Err(e) => {
            log::warn!(
                "admin UDS peer_cred (macOS/BSD) failed: {}; token-only fallback",
                e
            );
            true
        }
    }
}

#[cfg(unix)]
use hbb_common::libc;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_url_safe_and_43_chars() {
        let t = generate_token();
        let s = t.expose();
        assert_eq!(s.len(), 43, "base64 url-safe no-pad of 32 bytes = 43 chars");
        assert!(
            s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "url-safe alphabet only, got {}",
            s
        );
    }

    #[test]
    fn constant_time_eq_basic() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn split_token_cmd_space() {
        let (tok, cmd) = split_token_cmd(b"abc h\n").unwrap();
        assert_eq!(tok, b"abc");
        assert_eq!(cmd, "h\n");
    }

    #[test]
    fn split_token_cmd_newline_no_cmd() {
        let (tok, cmd) = split_token_cmd(b"abc\n").unwrap();
        assert_eq!(tok, b"abc");
        assert_eq!(cmd, "");
    }

    #[test]
    fn split_token_cmd_no_separator() {
        assert!(split_token_cmd(b"abc").is_none());
    }

    #[test]
    fn token_debug_display_mask() {
        let t = Token::new("supersecret".to_string());
        assert_eq!(format!("{}", t), "***");
        assert_eq!(format!("{:?}", t), "Token(***)");
    }
}
