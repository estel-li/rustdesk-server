// CE-M0-6: tcp_punch HashMap 的强类型 key 与 GC 辅助。
//
// 修复点:
//   旧实现以 `SocketAddr`(IP+port)作为 tcp_punch 的 key。在 NAT 后多设备共享
//   同一出口端口、或客户端重连复用端口的场景下,两条独立的 punch hole / relay
//   请求会互相覆盖,导致响应路由到错误的会话。
//
// 新实现把 peer_id 拼进 key,确保唯一性;同时为每条 sink 记录入表时间,由
// 后台 GC 60s 一次清理超时条目,避免连接异常断开时遗留垃圾。

use hbb_common::try_into_v4;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::Instant;

/// CE-M0-6: 用于 tcp_punch HashMap 的强类型 key。
/// 仅 ip+port 不足以唯一标识一条挂起的 punch hole 请求,
/// 加上 peer_id 防止 NAT 后多设备共享出口端口时互踩。
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub(crate) struct TcpPunchKey {
    pub ip: IpAddr,
    pub port: u16,
    pub peer_id: String, // 即 PunchHoleRequest.id / RequestRelay.id
}

impl TcpPunchKey {
    /// 用 `(addr, peer_id)` 构造 key。`addr` 会先 v6→v4 折叠,与旧实现一致。
    pub fn new(addr: SocketAddr, peer_id: impl Into<String>) -> Self {
        let addr = try_into_v4(addr);
        Self {
            ip: addr.ip(),
            port: addr.port(),
            peer_id: peer_id.into(),
        }
    }

    /// 该 key 对应的 (ip, port) 元组,供 fallback 反向查表使用。
    #[inline]
    #[allow(dead_code)]
    pub fn ip_port(&self) -> (IpAddr, u16) {
        (self.ip, self.port)
    }
}

impl std::fmt::Display for TcpPunchKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 不打印 peer_id 明文,只摘要前 4 字符,避免日志泄露 ID。
        let pid = if self.peer_id.len() > 4 {
            &self.peer_id[..4]
        } else {
            &self.peer_id
        };
        write!(f, "{}:{}#{}", self.ip, self.port, pid)
    }
}

/// CE-M0-6: tcp_punch 表中 sink 的载荷,带入表时间用于 GC。
/// 直接抽到独立模块是为了在 helpers 中做纯函数单测。
pub(crate) struct TcpPunchEntry<S> {
    pub sink: S,
    pub inserted_at: Instant,
}

impl<S> TcpPunchEntry<S> {
    #[inline]
    pub fn new(sink: S) -> Self {
        Self {
            sink,
            inserted_at: Instant::now(),
        }
    }
}

// ---------------------------------------------------------------------------
// 纯函数 helpers
//
// 抽成不依赖 RendezvousServer 整体状态的形式,方便单测覆盖,也方便后续如果
// 引入双索引(key->entry, (ip,port)->Vec<key>)时改造。
// ---------------------------------------------------------------------------

pub(crate) mod helpers {
    use super::*;

    /// CE-M0-6: 清理 `map` 中 `inserted_at` 超过 `ttl_secs` 的条目,返回被剔除条数。
    pub(crate) fn gc_tcp_punch<S>(
        map: &mut HashMap<TcpPunchKey, TcpPunchEntry<S>>,
        ttl_secs: u64,
    ) -> usize {
        let stale: Vec<TcpPunchKey> = map
            .iter()
            .filter_map(|(k, v)| {
                if v.inserted_at.elapsed().as_secs() >= ttl_secs {
                    Some(k.clone())
                } else {
                    None
                }
            })
            .collect();
        let n = stale.len();
        for k in stale {
            map.remove(&k);
        }
        n
    }

    /// CE-M0-6: 当只持有 `addr`(不带 peer_id)时,按 `(ip, port)` 反查 sink。
    /// 一个 addr 同一时刻通常只挂一条请求,直接 remove 首个匹配条目即可。
    /// 注:这条路径只在 RelayResponse / hole_sent 等少数旧入口走;
    ///     真正大流量需要 O(1) 反查时,后续工单可补 (ip,port)->key 双索引。
    pub(crate) fn remove_by_addr<S>(
        map: &mut HashMap<TcpPunchKey, TcpPunchEntry<S>>,
        addr: SocketAddr,
    ) -> Option<TcpPunchEntry<S>> {
        let addr = try_into_v4(addr);
        let ip = addr.ip();
        let port = addr.port();
        let hit: Option<TcpPunchKey> = map.keys().find(|k| k.ip == ip && k.port == port).cloned();
        hit.and_then(|k| map.remove(&k))
    }

    /// CE-M0-6: 兜底:连接关闭时按 `(ip, port)` 清掉所有挂在该 addr 上的 sink。
    /// 此时已无法定位 peer_id,只能按 (ip, port) 全清,与旧版语义对齐。
    pub(crate) fn remove_all_by_addr<S>(
        map: &mut HashMap<TcpPunchKey, TcpPunchEntry<S>>,
        addr: SocketAddr,
    ) -> usize {
        let addr = try_into_v4(addr);
        let ip = addr.ip();
        let port = addr.port();
        let stale: Vec<TcpPunchKey> = map
            .keys()
            .filter(|k| k.ip == ip && k.port == port)
            .cloned()
            .collect();
        let n = stale.len();
        for k in stale {
            map.remove(&k);
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4, SocketAddrV6};
    use std::time::Duration;

    fn v4(ip: [u8; 4], port: u16) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(ip), port))
    }

    #[test]
    fn test_v6_mapped_collapses_to_v4() {
        let v4_sa = v4([192, 168, 1, 2], 1234);
        // v6-mapped form of the same v4.
        let v6_mapped = SocketAddr::V6(SocketAddrV6::new(
            Ipv4Addr::new(192, 168, 1, 2).to_ipv6_mapped(),
            1234,
            0,
            0,
        ));
        let k_v4 = TcpPunchKey::new(v4_sa, "abc");
        let k_v6 = TcpPunchKey::new(v6_mapped, "abc");
        assert_eq!(k_v4, k_v6, "v6-mapped 与原生 v4 应折叠到同一 key");
        assert!(matches!(k_v6.ip, IpAddr::V4(_)));
    }

    #[test]
    fn test_different_peer_id_distinct() {
        let addr = v4([10, 0, 0, 1], 4321);
        let k_a = TcpPunchKey::new(addr, "peer_a");
        let k_b = TcpPunchKey::new(addr, "peer_b");
        assert_ne!(k_a, k_b);
        // 哈希到不同桶的概率极大;严格地说 Hash 允许碰撞,但实现上稳定 != 即可。
        let mut m = HashMap::new();
        m.insert(k_a.clone(), 1u32);
        m.insert(k_b.clone(), 2u32);
        assert_eq!(m.len(), 2);
        assert_eq!(m.get(&k_a), Some(&1));
        assert_eq!(m.get(&k_b), Some(&2));
    }

    #[test]
    fn test_eq_hash_roundtrip() {
        let addr = v4([127, 0, 0, 1], 21116);
        let k = TcpPunchKey::new(addr, "id-1");
        let k_clone = k.clone();
        let mut m: HashMap<TcpPunchKey, &'static str> = HashMap::new();
        m.insert(k, "sink");
        assert_eq!(m.get(&k_clone), Some(&"sink"));
    }

    // 用 unit-typed sink 跑 helpers 上的纯函数测试,避免拽进 tokio。
    #[test]
    fn test_gc_tcp_punch_evicts_stale() {
        let mut m: HashMap<TcpPunchKey, TcpPunchEntry<()>> = HashMap::new();
        let k_old = TcpPunchKey::new(v4([1, 2, 3, 4], 100), "old");
        let k_new = TcpPunchKey::new(v4([1, 2, 3, 4], 101), "new");
        // 构造一个已过期的 inserted_at
        let past = Instant::now()
            .checked_sub(Duration::from_secs(120))
            .unwrap_or_else(Instant::now);
        m.insert(
            k_old.clone(),
            TcpPunchEntry {
                sink: (),
                inserted_at: past,
            },
        );
        m.insert(k_new.clone(), TcpPunchEntry::new(()));
        let evicted = helpers::gc_tcp_punch(&mut m, 60);
        assert_eq!(evicted, 1);
        assert!(!m.contains_key(&k_old));
        assert!(m.contains_key(&k_new));
    }

    #[test]
    fn test_remove_by_addr_no_peer_id() {
        let mut m: HashMap<TcpPunchKey, TcpPunchEntry<u32>> = HashMap::new();
        let addr = v4([10, 0, 0, 5], 5555);
        let k = TcpPunchKey::new(addr, "peer_x");
        m.insert(k.clone(), TcpPunchEntry::new(7u32));
        let hit = helpers::remove_by_addr(&mut m, addr);
        assert!(hit.is_some());
        assert_eq!(hit.unwrap().sink, 7);
        // 再 remove 应返回 None,不 panic。
        assert!(helpers::remove_by_addr(&mut m, addr).is_none());
    }

    #[test]
    fn test_remove_all_by_addr_two_devices_same_nat() {
        // 失败模式 1 的回归测试:同 IP + 同 port + 不同 peer_id 共存,
        // 连接关闭兜底应该一次性清掉所有挂在该 addr 上的条目。
        let mut m: HashMap<TcpPunchKey, TcpPunchEntry<u8>> = HashMap::new();
        let addr = v4([100, 64, 0, 1], 33333);
        let k_a = TcpPunchKey::new(addr, "device_a");
        let k_b = TcpPunchKey::new(addr, "device_b");
        m.insert(k_a.clone(), TcpPunchEntry::new(1));
        m.insert(k_b.clone(), TcpPunchEntry::new(2));
        assert_eq!(m.len(), 2, "key 含 peer_id,应保留两条互不覆盖");
        let n = helpers::remove_all_by_addr(&mut m, addr);
        assert_eq!(n, 2);
        assert!(m.is_empty());
    }
}
