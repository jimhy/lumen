//! M6 P2P · 极简 STUN 反射端（RFC 5389 子集）。
//!
//! 独立 UDP 端点，与中继 WS（TCP `bind_addr`）解耦：收 Binding Request 即回 Binding Success
//! Response，XOR-MAPPED-ADDRESS = 请求**源地址**。客户端 `lumen-app::p2p` 经此探自己的公网映射
//! 端点（NAT 外侧 ip:port），用于 QUIC 打洞候选交换（见 `docs/M6-P2P直连-QUIC打洞-设计-2026-06-23.md` §7）。
//!
//! **替代被墙的公共 STUN**（国内可达 + 自主可控）。仅处理 IPv4 源（Phase 1）；非法 / 非 IPv4
//! 请求静默丢弃。无状态、无鉴权（STUN 反射本就是公开反射，不泄露任何服务端机密）。

use std::net::SocketAddr;

use tokio::net::UdpSocket;

/// RFC 5389 magic cookie。
const MAGIC_COOKIE: u32 = 0x2112_A442;

/// 启动 STUN 反射端（长驻 UDP 收发循环）。`bind_addr` 形如 `0.0.0.0:8788`。
///
/// # Errors
/// 绑定 UDP 端口失败时返回（如端口被占）。绑定后进入无限循环，仅在 socket 致命错误时返回。
pub async fn serve(bind_addr: &str) -> anyhow::Result<()> {
    let sock = UdpSocket::bind(bind_addr).await?;
    tracing::info!("STUN 反射端就绪 → udp://{bind_addr}");
    let mut buf = [0u8; 512];
    loop {
        let (n, src) = match sock.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!("STUN recv 失败: {e}");
                continue;
            }
        };
        if let Some(resp) = build_binding_response(&buf[..n], src) {
            let _ = sock.send_to(&resp, src).await;
        }
    }
}

/// 校验 Binding Request（type 0x0001 + magic cookie）并构造 Binding Success Response（0x0101），
/// 携 XOR-MAPPED-ADDRESS = `src`（仅 IPv4）。非法请求 / 非 IPv4 源 → `None`（丢弃）。
fn build_binding_response(req: &[u8], src: SocketAddr) -> Option<Vec<u8>> {
    if req.len() < 20 {
        return None;
    }
    if u16::from_be_bytes([req[0], req[1]]) != 0x0001 {
        return None; // 仅认 Binding Request
    }
    if u32::from_be_bytes([req[4], req[5], req[6], req[7]]) != MAGIC_COOKIE {
        return None;
    }
    let txn = &req[8..20];
    let SocketAddr::V4(v4) = src else {
        return None; // IPv6 留待后续阶段
    };
    let ip = v4.ip().octets();
    let cookie = MAGIC_COOKIE.to_be_bytes();
    let x_port = v4.port() ^ (MAGIC_COOKIE >> 16) as u16;
    let x_addr = [
        ip[0] ^ cookie[0],
        ip[1] ^ cookie[1],
        ip[2] ^ cookie[2],
        ip[3] ^ cookie[3],
    ];
    let mut resp = Vec::with_capacity(32);
    resp.extend_from_slice(&0x0101u16.to_be_bytes()); // Binding Success Response
    resp.extend_from_slice(&12u16.to_be_bytes()); // 属性总长 = 4 头 + 8 值
    resp.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
    resp.extend_from_slice(txn);
    resp.extend_from_slice(&0x0020u16.to_be_bytes()); // XOR-MAPPED-ADDRESS
    resp.extend_from_slice(&8u16.to_be_bytes());
    resp.push(0x00); // reserved
    resp.push(0x01); // family IPv4
    resp.extend_from_slice(&x_port.to_be_bytes());
    resp.extend_from_slice(&x_addr);
    Some(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    /// 构造 Binding Request → 反射 → 按 RFC 5389 XOR 解码，应还原源地址（与客户端 p2p.rs 对称）。
    #[test]
    fn 反射_xor_mapped_往返_ipv4() {
        let txn = [3u8; 12];
        let mut req = [0u8; 20];
        req[0..2].copy_from_slice(&0x0001u16.to_be_bytes());
        req[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
        req[8..20].copy_from_slice(&txn);
        let src: SocketAddr = "198.51.100.7:40000".parse().expect("源地址");
        let resp = build_binding_response(&req, src).expect("应回响应");

        assert_eq!(u16::from_be_bytes([resp[0], resp[1]]), 0x0101);
        assert_eq!(&resp[8..20], &txn, "transaction id 原样回带");
        // XOR-MAPPED-ADDRESS 值：x-port @ [26..28]，x-addr @ [28..32]。
        let port = u16::from_be_bytes([resp[26], resp[27]]) ^ (MAGIC_COOKIE >> 16) as u16;
        let cookie = MAGIC_COOKIE.to_be_bytes();
        let ip = Ipv4Addr::new(
            resp[28] ^ cookie[0],
            resp[29] ^ cookie[1],
            resp[30] ^ cookie[2],
            resp[31] ^ cookie[3],
        );
        assert_eq!(port, 40000);
        assert_eq!(ip, Ipv4Addr::new(198, 51, 100, 7));
    }

    #[test]
    fn 非法请求一律丢弃() {
        let src: SocketAddr = "198.51.100.7:40000".parse().expect("源地址");
        // 过短。
        assert!(build_binding_response(&[0u8; 8], src).is_none());
        // 错误消息类型。
        let mut wrong_type = [0u8; 20];
        wrong_type[0..2].copy_from_slice(&0x0101u16.to_be_bytes());
        wrong_type[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
        assert!(build_binding_response(&wrong_type, src).is_none());
        // 错误 magic cookie。
        let mut bad_cookie = [0u8; 20];
        bad_cookie[0..2].copy_from_slice(&0x0001u16.to_be_bytes());
        assert!(build_binding_response(&bad_cookie, src).is_none());
    }
}
