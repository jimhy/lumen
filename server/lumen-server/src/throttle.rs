//! 登录 / 注册节流（片 11）——进程内滑动窗口，不引 tower-governor。
//!
//! # 为什么手写 30 行而不引中间件
//!
//! 需求只有「同一个键在 N 秒内至多 M 次」，而 tower-governor 会带进
//! `governor` + `dashmap` + `nonzero_ext` 一串依赖，还要适配它的 key extractor。
//! 蓝图 §8.2-5 明写「30 行的进程内滑动窗口，不必引 tower-governor」。
//!
//! **进程内**意味着多实例部署时每个实例各算各的。这是已知取舍：本服务是单实例形态
//! （见 `server/deploy/`），真要横向扩容时节流得整体换成共享存储，那时这个模块整个替换。
//!
//! # ★ 只对**失败**计数
//!
//! 成功即清零。于是正常用户几乎不可能撞上限——撞上的只有连续输错密码的人和爆破脚本，
//! 而这正是想挡的两类。若对所有尝试计数，一个手滑的用户会被自己的正常重试锁在门外。
//!
//! # ★ IP 维度会**自动失效**，而且它必须自动失效
//!
//! 见 [`ClientIp`]：在反代后面时 socket 对端恒为回环地址，若拿它当 key，
//! **全服务共用一个桶** —— 5 次/5 分钟就是整个服务每 5 分钟只能有 5 次登录尝试。
//! 那不是节流，那是拒绝服务。所以拿不到真实 IP 时宁可让 IP 维度不生效。

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;

use axum::http::HeaderMap;

/// 窗口长度（秒）。
pub const WINDOW_SECS: i64 = 300;

/// 同一邮箱在窗口内允许的**失败**次数。
///
/// 5 次覆盖「记错密码试几遍」，而爆破需要的量级远在其上。
pub const MAX_FAILS_PER_EMAIL: usize = 5;

/// 同一 IP 在窗口内允许的失败次数。
///
/// 比邮箱维度松：一个家庭/办公网络出口后面可能有好几个人，而且**换邮箱遍历**才是
/// 这一维要挡的形态（邮箱维度对它无效——每次换邮箱等于换一个新桶）。
pub const MAX_FAILS_PER_IP: usize = 30;

/// 信任反向代理头的开关（`=1` 生效）。
pub const TRUST_PROXY_ENV: &str = "LUMEN_TRUST_PROXY_HEADER";

/// 一次请求的客户端 IP 判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientIp {
    /// 拿到了可信的客户端地址。
    Known(IpAddr),
    /// **拿不到**：在反代后面且没开 [`TRUST_PROXY_ENV`]。IP 维度此时整个跳过。
    Opaque,
}

/// 从连接信息与请求头判定客户端 IP。
///
/// | 部署形态 | `LUMEN_TRUST_PROXY_HEADER` | 结果 |
/// |---|---|---|
/// | Caddy 反代（标准形态） | `1` | `X-Forwarded-For` 的**最后一跳** |
/// | Caddy 反代 | 未设 | [`ClientIp::Opaque`]（对端是回环，拿了也是全服务一个桶） |
/// | 直接暴露 8787 | 未设 | socket 对端（就是真实客户端） |
///
/// # ★ 取 `X-Forwarded-For` 的**最后一个**，不是第一个
///
/// 这个头的形状是 `客户端, 代理1, 代理2`，**前面的部分由客户端自己发、可以随便伪造**。
/// 每一跳代理会把它看到的对端追加到末尾，所以最后一个才是紧邻本服务的那一跳看到的
/// 真实地址。取第一个 = 把 key 的选择权交给攻击者，节流当场失效。
pub fn client_ip(peer: Option<SocketAddr>, headers: &HeaderMap) -> ClientIp {
    let trust = std::env::var(TRUST_PROXY_ENV).as_deref() == Ok("1");
    if trust {
        if let Some(ip) = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(last_hop)
        {
            return ClientIp::Known(ip);
        }
        // 开了信任却没有这个头：可能是有人绕过反代直连。落回对端地址。
    }
    match peer.map(|p| p.ip()) {
        // 对端是回环 = 我们在反代（或隧道）后面，这个地址对所有人都一样。
        Some(ip) if ip.is_loopback() => ClientIp::Opaque,
        Some(ip) => ClientIp::Known(ip),
        None => ClientIp::Opaque,
    }
}

/// 取 `X-Forwarded-For` 的最后一跳。
fn last_hop(raw: &str) -> Option<IpAddr> {
    raw.rsplit(',').map(str::trim).find_map(|s| {
        s.parse::<IpAddr>()
            .ok()
            // `[::1]:1234` 这种带端口的形式也认一下。
            .or_else(|| s.parse::<SocketAddr>().ok().map(|a| a.ip()))
    })
}

/// 滑动窗口计数器。
#[derive(Debug, Default)]
pub struct Throttle {
    /// `key → 该键最近的失败时刻（升序）`。
    hits: Mutex<HashMap<String, Vec<i64>>>,
}

/// 检查结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// 放行。
    Allow,
    /// 拒绝，并建议等这么多秒。
    Deny {
        /// 最老的那次失败滑出窗口还要多久。
        retry_after_secs: u64,
    },
}

impl Throttle {
    /// 新建。
    pub fn new() -> Self {
        Self::default()
    }

    /// 这次请求该不该放行。**只查不记**——记一笔是 [`Self::record_failure`] 的事。
    ///
    /// 查与记分开，是因为「成功的登录不该计数」：handler 要先查、再验密码、
    /// 只在验失败时记。
    pub fn check(&self, key: &str, limit: usize, now: i64) -> Verdict {
        let mut hits = self.lock();
        let Some(times) = hits.get_mut(key) else {
            return Verdict::Allow;
        };
        prune(times, now);
        if times.len() < limit {
            return Verdict::Allow;
        }
        // 最老的那次滑出窗口时即可再试。
        let oldest = times.first().copied().unwrap_or(now);
        let wait = (oldest + WINDOW_SECS - now).max(1);
        Verdict::Deny {
            retry_after_secs: wait as u64,
        }
    }

    /// 记一次失败。
    pub fn record_failure(&self, key: &str, now: i64) {
        let mut hits = self.lock();
        let times = hits.entry(key.to_string()).or_default();
        prune(times, now);
        times.push(now);
    }

    /// 成功即清零——正常用户不该被自己之前的手滑挡住。
    pub fn clear(&self, key: &str) {
        self.lock().remove(key);
    }

    /// 周期清理：丢掉整个窗口内都没有活动的键。
    ///
    /// **必须有**：`hits` 的键来自请求（邮箱、IP），不清就是一条随请求量无界增长的
    /// 内存路径——而登录接口恰恰是未鉴权可达的。
    pub fn gc(&self, now: i64) {
        let mut hits = self.lock();
        hits.retain(|_, times| {
            prune(times, now);
            !times.is_empty()
        });
    }

    /// 当前跟踪的键数（测试与指标用）。
    pub fn tracked_keys(&self) -> usize {
        self.lock().len()
    }

    /// 锁中毒时不 panic：节流是防护层，它自己把服务打挂就本末倒置了。
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<i64>>> {
        self.hits.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 丢掉窗口外的记录。
fn prune(times: &mut Vec<i64>, now: i64) {
    let cutoff = now - WINDOW_SECS;
    times.retain(|t| *t > cutoff);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 片11_窗口内超限即拒并给出等待秒数() {
        let t = Throttle::new();
        for i in 0..MAX_FAILS_PER_EMAIL {
            assert_eq!(t.check("a@b.c", MAX_FAILS_PER_EMAIL, 1000), Verdict::Allow);
            t.record_failure("a@b.c", 1000 + i as i64);
        }
        let Verdict::Deny { retry_after_secs } = t.check("a@b.c", MAX_FAILS_PER_EMAIL, 1000) else {
            panic!("第 {} 次必须被拒", MAX_FAILS_PER_EMAIL + 1);
        };
        // 只说「太频繁」而不说等多久，客户端只能盲目重试 —— 那正是节流想挡的行为。
        assert_eq!(retry_after_secs, WINDOW_SECS as u64);
    }

    #[test]
    fn 片11_窗口滑过之后自动恢复() {
        let t = Throttle::new();
        for _ in 0..MAX_FAILS_PER_EMAIL {
            t.record_failure("a@b.c", 1000);
        }
        assert!(matches!(
            t.check("a@b.c", MAX_FAILS_PER_EMAIL, 1000),
            Verdict::Deny { .. }
        ));
        assert_eq!(
            t.check("a@b.c", MAX_FAILS_PER_EMAIL, 1000 + WINDOW_SECS + 1),
            Verdict::Allow
        );
    }

    #[test]
    fn 片11_成功登录清零() {
        // 不清零的话，一个记错密码试了四次、第五次终于对了的用户，
        // 接下来 5 分钟内再登一次就会被自己之前的手滑挡在门外。
        let t = Throttle::new();
        for _ in 0..MAX_FAILS_PER_EMAIL {
            t.record_failure("a@b.c", 1000);
        }
        t.clear("a@b.c");
        assert_eq!(t.check("a@b.c", MAX_FAILS_PER_EMAIL, 1000), Verdict::Allow);
    }

    #[test]
    fn 片11_不同键互不影响() {
        let t = Throttle::new();
        for _ in 0..MAX_FAILS_PER_EMAIL {
            t.record_failure("a@b.c", 1000);
        }
        assert_eq!(t.check("x@y.z", MAX_FAILS_PER_EMAIL, 1000), Verdict::Allow);
    }

    #[test]
    fn 片11_gc清掉不活跃的键() {
        // 键来自未鉴权可达的登录接口，不清就是一条随请求量无界增长的内存路径。
        let t = Throttle::new();
        t.record_failure("a@b.c", 1000);
        t.record_failure("d@e.f", 1000);
        assert_eq!(t.tracked_keys(), 2);
        t.gc(1000 + WINDOW_SECS + 1);
        assert_eq!(t.tracked_keys(), 0);
    }

    // ── 客户端 IP 判定 ───────────────────────────────────────────────────────

    fn xff(v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", v.parse().expect("header"));
        h
    }

    fn addr(s: &str) -> Option<SocketAddr> {
        Some(s.parse().expect("addr"))
    }

    /// 客户端 IP 判定的四种情形。
    ///
    /// ⚠ **必须挤在一个 `#[test]` 里**：它们改的是**进程级**环境变量，而 Rust 测试默认
    /// 多线程并行——拆成四个会互相踩，表现为随机失败（本片第一版就是这么红的）。
    #[test]
    fn 片11_客户端ip判定的四种情形() {
        // ① 直接暴露 8787：socket 对端就是真实客户端。
        std::env::remove_var(TRUST_PROXY_ENV);
        assert_eq!(
            client_ip(addr("203.0.113.9:5000"), &HeaderMap::new()),
            ClientIp::Known("203.0.113.9".parse().expect("ip"))
        );

        // ② ★ 反代后没开信任 ⇒ IP 维度整个失效。
        //    拿回环地址当 key = 全服务共用一个桶 = 每 5 分钟只允许 5 次登录。
        //    那不是节流，那是拒绝服务。宁可让这一维不生效。
        assert_eq!(
            client_ip(addr("127.0.0.1:5000"), &xff("203.0.113.9")),
            ClientIp::Opaque
        );
        assert_eq!(client_ip(None, &HeaderMap::new()), ClientIp::Opaque);

        // ③ ★ 信任反代时取 XFF 的**最后一跳**，不是第一个。
        //    这个头的形状是 `客户端, 代理1, 代理2`，前面的部分由客户端自己发、
        //    可以随便伪造。取第一个 = 把 key 的选择权交给攻击者，节流当场失效。
        std::env::set_var(TRUST_PROXY_ENV, "1");
        assert_eq!(
            client_ip(addr("127.0.0.1:5000"), &xff("1.2.3.4, 203.0.113.9")),
            ClientIp::Known("203.0.113.9".parse().expect("ip"))
        );
        // 开了信任但没有这个头（有人绕过反代直连）⇒ 落回对端地址。
        assert_eq!(
            client_ip(addr("127.0.0.1:5000"), &HeaderMap::new()),
            ClientIp::Opaque
        );

        // ④ XFF 里的垃圾值被跳过，继续往前找。
        assert_eq!(
            client_ip(addr("10.0.0.1:1"), &xff("203.0.113.9, garbage")),
            ClientIp::Known("203.0.113.9".parse().expect("ip")),
        );

        // ⑤ 含非 ASCII 的头**整个不可读**（`HeaderValue::to_str` 对 obs-text 直接失败），
        //    于是落回对端地址。这是安全方向的降级 —— 不会退化成「取第一个」。
        //    单独钉一条是因为它不显然：第一版测试拿中文当垃圾值，撞的其实是这条路径，
        //    差点把「跳过垃圾值」误判成已覆盖。
        assert_eq!(
            client_ip(addr("10.0.0.1:1"), &xff("203.0.113.9, 不是地址")),
            ClientIp::Known("10.0.0.1".parse().expect("ip")),
        );

        std::env::remove_var(TRUST_PROXY_ENV);
    }
}
