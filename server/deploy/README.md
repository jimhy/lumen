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

## 注意

- `LUMEN_JWT_SECRET` **必须**改强随机，否则 token 可被伪造。
- server ↔ Postgres 当前 `NoTls`；若跨主机需加密，接 tokio-postgres 的 rustls connector。
- 8788/udp 若被云安全组/NAT 挡住，P2P 打洞失败会自动回退 WebSocket 中继（功能不受影响、仅非直连）。
