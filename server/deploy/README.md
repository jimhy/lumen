# Lumen Server 云部署（Linux）

生产部署 `lumen-server` 到云 Linux 的完整步骤。本目录附：

- `lumen-server.service` — systemd 服务单元
- `lumen-server.env.example` — 环境变量模板（`LUMEN_*`）
- `Caddyfile.example` — Caddy 反代（对外 HTTPS → 内网明文）

技术栈：Rust + tokio + axum + tokio-postgres。**服务端启动自动幂等建表**
（`CREATE TABLE IF NOT EXISTS`），无需手动跑 SQL。服务端**不依赖 quinn**，无客户端
那个 release 构建的 rustc 崩溃。

---

## 1. 端口与网络

| 端口 | 协议 | 用途 | 对外暴露 |
|---|---|---|---|
| 8787 | TCP | WebSocket 远程控制中继 + REST API | 经 Caddy 反代（对外只开 443） |
| 8788 | UDP | M6 P2P 打洞 STUN 反射端 | **必须公网直接可达**（不走反代） |

防火墙放行：`443/tcp`（Caddy HTTPS）、`8788/udp`（P2P）。若不用反代直接暴露，则放 `8787/tcp`。

## 2. Postgres

```bash
sudo -u postgres psql -c "CREATE USER lumen_user WITH PASSWORD 'CHANGE_ME_DB_PASSWORD';"
sudo -u postgres psql -c "CREATE DATABASE lumen OWNER lumen_user;"
```

（建表由服务端启动时自动完成，无需手动建表。）

## 3. 构建

在云 Linux 上只编 server（不触及 Windows-only 客户端 crate）：

```bash
git clone git@github.com:jimhy/lumen.git
cd lumen
git checkout v1.0.0        # 或 main
cargo build -p lumen-server --release
# 产物：target/release/lumen-server
sudo mkdir -p /opt/lumen-server
sudo cp target/release/lumen-server /opt/lumen-server/
```

## 4. 配置

```bash
sudo useradd --system --no-create-home --shell /usr/sbin/nologin lumen   # 服务账户
sudo mkdir -p /etc/lumen-server
sudo cp server/deploy/lumen-server.env.example /etc/lumen-server/lumen-server.env
sudo chmod 600 /etc/lumen-server/lumen-server.env
# 编辑，逐项改（尤其 LUMEN_JWT_SECRET / LUMEN_DATABASE_URL）：
#   LUMEN_JWT_SECRET=$(openssl rand -hex 32)
sudo nano /etc/lumen-server/lumen-server.env
```

> ### ⚠ 破坏性变更（M7 片 11 起）：不设 `LUMEN_JWT_SECRET` 会**拒绝启动**
>
> 旧版本只打一条启动 warn 就继续跑。现在改成直接退出，因为默认密钥是**写在源码里的
> 公开字符串**，而本服务的 JWT 无 `jti`、无版本号、改密码不失效、TTL 7 天——
> 用默认密钥等于任何人都能给任意账户签一张 7 天有效的通行证。
>
> **从旧版本升级时**：先确认 `/etc/lumen-server/lumen-server.env` 里有这一行，
> 再重启服务。没有就会起不来（`journalctl -u lumen-server` 里有完整提示与命令）。
>
> ```bash
> grep -q '^LUMEN_JWT_SECRET=' /etc/lumen-server/lumen-server.env \
>   || echo "LUMEN_JWT_SECRET=$(openssl rand -hex 32)" | sudo tee -a /etc/lumen-server/lumen-server.env
> ```
>
> ⚠ **如果你此前一直在用默认密钥**，那么这次设置新密钥会让**所有已签发的 token 立刻失效**
> ——每台客户端都要重新登录一次。这是预期行为：那些 token 本来就是任何人都能伪造的。
>
> 本地开发 / CI 确实要用默认密钥时，显式放行（**切勿用于公网**）：
>
> ```bash
> LUMEN_ALLOW_INSECURE_SECRET=1
> ```
>
> ⚠ **`LUMEN_JWT_SECRET` 只在首次部署生成一次，之后务必固定不变。**
> 升级 / 重部署时**不要重跑 `openssl rand`** 覆盖它——密钥一变，所有客户端的
> 现有 token 立刻验签失败（全端 401），而客户端此前不提示、只默默显示「未连接」，
> 用户须在每台机手动重新登录才能恢复互相可见（本项目已修复的历史 bug 的诱因之一）。
> 同理，`LUMEN_DATABASE_URL` 指向的库必须持久化（本文档用宿主机原生 Postgres，
> 天然持久；**切勿**用无卷的一次性容器，否则设备/账户数据随重启丢失）。

## 5. systemd

```bash
sudo cp server/deploy/lumen-server.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now lumen-server
sudo systemctl status lumen-server
journalctl -u lumen-server -f          # 看日志
```

## 6. TLS（Caddy 反代）

```bash
sudo cp server/deploy/Caddyfile.example /etc/caddy/Caddyfile
sudo nano /etc/caddy/Caddyfile         # 改成你的域名
sudo systemctl reload caddy
```

Caddy 自动申请/续期 Let's Encrypt 证书。REST + WebSocket 共用同一 HTTPS 入口。

## 7. 客户端指向

客户端**不预设默认服务端地址**——用户在 Lumen「设置」里的「服务端地址」填
`https://你的域名`（一次持久化）。开发/自测可用环境变量 `LUMEN_SERVER_URL` 覆盖。

## 8. 验证

```bash
# 本机探活（REST）
curl -sS http://127.0.0.1:8787/health || true    # 若有 health 路由
# 客户端登录 → 设备列表出现 → 远程控制 / P2P 打洞可用即成功。
```

## 9. 升级与设备身份（hw_id 迁移 / 幽灵清理）

服务端设备身份已引入**稳定硬件标识** `hw_id`（客户端 Windows `MachineGuid`），据
`(user_id, hw_id)` 幂等认领同一物理机，杜绝「客户端带空/异 `device_id` 就分裂出重复
（幽灵）设备行」。

- **迁移全自动、无需人工**：服务端启动时按既有「幂等 DDL」约定自动 `ALTER TABLE
  devices ADD COLUMN IF NOT EXISTS hw_id` + 建部分唯一索引；历史行 `hw_id=NULL`、
  与新逻辑平滑共存。每台机下次带 `hw_id` 登录时由 upsert 自动回填其行、复用老行、
  `id` 不变（滚动升级期零新增幽灵）。**升级只需换二进制 + 重启**，不必停机 / 手工刷库。
- **新旧兼容**：老客户端不发 `hw_id`，服务端退化回原「按 `device_id` 更新，否则新建」；
  新客户端×老服务端时 `hw_id` 字段被忽略。可分别或同时升级。
- **清理既有幽灵（可选、手动、务必先看后删）**：升级后不再新增幽灵，历史遗留的重复
  离线行不影响使用（客户端设备列表本就只显示在线设备），如需清理，**先审后删**：

  ```sql
  -- 先看每个账户下的设备行（按机器名/最近活跃排序，人工识别哪些是重复的旧行）
  SELECT user_id, name, id, hw_id, last_seen, created_at
  FROM devices ORDER BY user_id, name, last_seen DESC;

  -- 确认后按 id 精确删除你判定为幽灵的行（最安全）：
  -- DELETE FROM devices WHERE id = '<ghost-device-id>';

  -- 或：删除「已回填 hw_id 的机器」名下、仍为 NULL 且早已离线的同名旧行（较激进，
  -- 有「两台真机同名」的误删风险，执行前务必用上面的 SELECT 复核）：
  -- DELETE FROM devices d WHERE d.hw_id IS NULL
  --   AND EXISTS (SELECT 1 FROM devices o
  --               WHERE o.user_id=d.user_id AND o.name=d.name AND o.hw_id IS NOT NULL);
  ```

  删除会经外键级联清掉该设备相关的配对信任（`device_pairs`），下次配对需重输一次配对码。

  **M7 片 11 起有了细粒度的撤销**，不必再靠删设备：

  | 想做的事 | 接口 |
  |---|---|
  | 看这台设备信任了哪些对端 | `GET /api/v1/pairs` |
  | 撤销与某台设备的信任 | `DELETE /api/v1/pairs/{peer_device_id}` |
  | 清空本设备的**全部**信任 | `DELETE /api/v1/pairs/*` |

  ⚠ 撤销只影响**下一次**建会话（届时要重新念配对码），**不会拆掉正在进行中的会话**。
  手机丢失的正确处置是「撤销 + 删设备」两步：前者断掉免码权，后者让那台设备的 token 失效。

## 注意

- `LUMEN_JWT_SECRET` **必须**改强随机，否则**服务端拒绝启动**（M7 片 11 起，见 §4）；
  且**一经设定不要再变**。
- **反代部署建议设 `LUMEN_TRUST_PROXY_HEADER=1`**（M7 片 11）：登录 / 注册的**按 IP 节流**
  靠它才能拿到真实客户端地址。不设的话，Caddy 后面的 socket 对端恒为 `127.0.0.1`，
  服务端会**主动放弃 IP 维度**（拿它当 key 就是全服务共用一个桶 = 每 5 分钟只允许 5 次登录）。
  此时按邮箱的那一维仍然生效，但「换邮箱遍历」与「批量注册」就没有防护了。
  ⚠ **只有在确认 8787 端口不对外暴露时才设它**——否则任何人都能伪造 `X-Forwarded-For`
  绕过节流（服务端取的是该头的**最后一跳**，绕过需要能直连 8787）。
- server ↔ Postgres 当前 `NoTls`；若跨主机需加密，接 tokio-postgres 的 rustls connector。
- 8788/udp 若被云安全组/NAT 挡住，P2P 打洞失败会自动回退 WebSocket 中继（功能不受影响、仅非直连）。
