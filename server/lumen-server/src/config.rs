//! 运行配置：全部从环境变量读取，带合理默认（本地 docker Postgres 开箱即用）。

use std::env;

/// 默认（不安全）JWT 密钥——仅供本地开发；生产必须经 `LUMEN_JWT_SECRET` 覆盖。
pub const DEFAULT_JWT_SECRET: &str = "dev-insecure-secret-change-me";

/// 显式放行默认密钥的逃生口（`=1` 生效）。
///
/// 没有它，本地开发与 CI 会被 [`Config::insecure_secret_refusal`] 直接挡在门外——而那两处
/// **本来就该**用默认密钥（不连公网、不存真数据）。留逃生口不是妥协：它把「我知道这不安全」
/// 变成一次**显式动作**，而不是一条谁都不会看的启动 warn。
pub const ALLOW_INSECURE_SECRET_ENV: &str = "LUMEN_ALLOW_INSECURE_SECRET";

/// 服务端运行配置。
#[derive(Debug, Clone)]
pub struct Config {
    /// Postgres 连接串。
    pub database_url: String,
    /// 监听地址（如 `0.0.0.0:8787`）。
    pub bind_addr: String,
    /// JWT 签名密钥（生产务必经 `LUMEN_JWT_SECRET` 覆盖）。
    pub jwt_secret: String,
    /// token 有效期（秒）。
    pub token_ttl_secs: i64,
    /// 设备在线判定阈值（秒）：`last_seen` 在此窗口内视为在线（M5.1 近似，M5.2 换心跳）。
    pub online_window_secs: i64,
    /// M6 P2P STUN 反射端 UDP 监听地址（如 `0.0.0.0:8788`）。客户端经此探公网映射端点做 QUIC
    /// 打洞（自建反射替代被墙的公共 STUN，国内可达 + 自主可控，见 docs/M6 设计 §7）。与中继 WS
    /// （TCP `bind_addr`）解耦的独立端点。
    pub stun_bind_addr: String,
}

impl Config {
    /// 从环境变量加载，缺失用默认值（默认对接本地 docker Postgres）。
    pub fn from_env() -> Self {
        Self {
            database_url: env::var("LUMEN_DATABASE_URL").unwrap_or_else(|_| {
                // 本地开发：专用 docker 容器 lumen-postgres（host 5544 -> 容器 5432）。
                // 用 127.0.0.1 强制 IPv4，避开本机原生 PostgreSQL 占用的 5432。
                // 详见 server/lumen-server/README.md。
                "postgres://lumen_user:lumen_password@127.0.0.1:5544/lumen?sslmode=disable"
                    .to_string()
            }),
            bind_addr: env::var("LUMEN_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8787".to_string()),
            jwt_secret: env::var("LUMEN_JWT_SECRET")
                .unwrap_or_else(|_| DEFAULT_JWT_SECRET.to_string()),
            token_ttl_secs: env::var("LUMEN_TOKEN_TTL_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(7 * 24 * 3600),
            // 在线窗口：last_seen 在此秒内视为在线。45s ≈ 客户端 10s 心跳的 4 次容差——离线后约
            // 45s 即被判离线、从控制端列表移除（120s 太久，海风哥反馈离线迟迟不消失）。
            online_window_secs: env::var("LUMEN_ONLINE_WINDOW_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(45),
            stun_bind_addr: env::var("LUMEN_STUN_BIND_ADDR")
                .unwrap_or_else(|_| "0.0.0.0:8788".to_string()),
        }
    }

    /// 是否仍在用默认（不安全）JWT 密钥。
    pub fn uses_default_jwt_secret(&self) -> bool {
        self.jwt_secret == DEFAULT_JWT_SECRET
    }

    /// 该不该因为默认密钥**拒绝启动**；返回 `Some(理由)` 即拒绝。
    ///
    /// # 为什么从「只 warn」升级成「拒启动」（片 11）
    ///
    /// 服务端签发的 JWT **无 `jti`、无版本号、改密码不失效**，默认 TTL 7 天，唯一的撤销
    /// 手段是删设备行。默认密钥是**公开在源码里的字符串** ⇒ 任何人都能给任意 `user_id`
    /// 签一个有效 7 天的 token，直接拿到该账户的全部设备与远程控制权。
    ///
    /// 一条启动 warn 挡不住这个：它滚在日志里，而部署脚本没人盯着看。
    ///
    /// # ⚠ 这是**破坏性变更**
    ///
    /// 已经用默认密钥跑起来的部署会在下次重启时起不来。迁移步骤写在
    /// `server/deploy/README.md`，发布说明必须同版给出。**注意换密钥会让所有已签发的
    /// token 失效**（客户端表现为要重新登录），那是预期行为、也是这次改动的意义之一。
    ///
    /// 判据只看环境变量、不看 `bind_addr` 是不是 `127.0.0.1`：绑本地也可能被反代到公网，
    /// 而「看起来只监听本地所以放行」正是这类判断最常见的错法。
    pub fn insecure_secret_refusal(&self) -> Option<String> {
        if !self.uses_default_jwt_secret() {
            return None;
        }
        if env::var(ALLOW_INSECURE_SECRET_ENV).as_deref() == Ok("1") {
            return None;
        }
        Some(format!(
            "拒绝启动：正在使用源码里公开的默认 JWT 密钥，任何人都能伪造本服务的登录凭据。\n\
             \n\
             生产部署请设置一个强随机密钥：\n\
             \n    LUMEN_JWT_SECRET=$(openssl rand -hex 32)\n\
             \n\
             ⚠ 它**一经设定不要再变**（换了会让全部已签发 token 失效，所有客户端需重新登录）。\n\
             \n\
             本地开发 / CI 确实要用默认密钥时，显式放行：\n\
             \n    {ALLOW_INSECURE_SECRET_ENV}=1\n"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个只关心密钥的配置（其余字段取默认值即可）。
    fn config_with(secret: &str) -> Config {
        Config {
            database_url: String::new(),
            bind_addr: "127.0.0.1:0".to_string(),
            jwt_secret: secret.to_string(),
            token_ttl_secs: 3600,
            online_window_secs: 45,
            stun_bind_addr: "127.0.0.1:0".to_string(),
        }
    }

    #[test]
    fn 片11_强密钥直接放行() {
        assert!(config_with("f".repeat(64).as_str())
            .insecure_secret_refusal()
            .is_none());
    }

    #[test]
    fn 片11_默认密钥拒绝启动且理由里带迁移命令() {
        let refusal = config_with(DEFAULT_JWT_SECRET)
            .insecure_secret_refusal()
            .expect("默认密钥必须被拒");
        // 拒绝信息**必须可操作**：只说「不安全」而不给命令，运维只会去把这条日志静音。
        assert!(refusal.contains("openssl rand -hex 32"), "{refusal}");
        assert!(refusal.contains(ALLOW_INSECURE_SECRET_ENV), "{refusal}");
        // 换密钥会踢掉所有人，这件事必须在拒绝信息里说，不能只写在文档里。
        assert!(refusal.contains("重新登录"), "{refusal}");
    }

    #[test]
    fn 片11_逃生口只认1不认随便什么真值() {
        // `LUMEN_ALLOW_INSECURE_SECRET=false` / `=0` / `=yes` 都不该放行 ——
        // 「设了就算数」会让一次手滑（比如写成 =0）变成静默的生产事故。
        //
        // ⚠ 本测试改进程级环境变量，与同进程其它测试并发时会互相干扰，
        //    故三种取值在**一个** #[test] 里顺序验完，不拆成三个。
        let cfg = config_with(DEFAULT_JWT_SECRET);
        for bad in ["0", "false", "yes", "true", ""] {
            // SAFETY 注：set_var 在多线程下是 unsafe（Rust 2024），本 crate 是
            // edition 2021，此处仍是安全 API；升级 edition 时要连同这段一起改。
            std::env::set_var(ALLOW_INSECURE_SECRET_ENV, bad);
            assert!(
                cfg.insecure_secret_refusal().is_some(),
                "{bad:?} 不该被当成放行"
            );
        }
        std::env::set_var(ALLOW_INSECURE_SECRET_ENV, "1");
        assert!(cfg.insecure_secret_refusal().is_none());
        std::env::remove_var(ALLOW_INSECURE_SECRET_ENV);
    }
}
