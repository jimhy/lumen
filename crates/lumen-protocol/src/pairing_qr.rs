//! 扫码配对的二维码载荷（M7 片 6）。
//!
//! # 这个模块**不上线缆**
//!
//! 它只经「PC 屏幕 → 手机摄像头」这条**带外信道**传递，永远不进 WebSocket、不进 REST。
//! 放在协议 crate 里的唯一理由是**两端必须逐字节一致**：PC 侧 `remote_pairing_qr.rs` 生成、
//! 手机侧 Dart 解析，任何一侧的字段名或校验顺序漂了，扫码要么失效、要么把安全校验放空。
//!
//! # 为什么明文、不签名、不加密
//!
//! 两端在扫码之前**没有任何共享密钥**——码本身就是那个一次性秘密。给它签名需要先有密钥，
//! 而密钥的分发问题正是这个码要解决的问题。签名在这里不增加任何安全性，只增加载荷长度
//! （而 QR 容量是硬约束）。
//!
//! # 扫码没有降低配对强度，它只是换了个呈现方式
//!
//! 9 位配对码的强度**不在「码有多长」**，而在服务端 `hub.rs::submit_pairing` 那一行
//! `p_owner != controller_id → NoPending`：**码只能由发起配对的那台设备提交**，且身份取自
//! JWT 里的 `did` 而非消息自报。所以把码渲染成二维码是**纯展示层替换**——pending 的 owner
//! 绑定、单次使用、5 次尝试、120 秒 TTL、同账户限制、被控端否决权**全部原样保留**，
//! 服务端零改动、协议版本不动。
//!
//! ⚠ **后人注意**：不要为了「扫码更方便」去做「被控端主动出码」（即不等 `PairingNeeded`
//! 就把码贴在屏幕上）。那会让 owner 绑定失效，整套强度塌掉。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 载荷的魔数。**解析后第一件事就是校验它**——没有它，任何一个二维码都会被当成配对码去试。
///
/// 带版本后缀：将来若要改字段，新版本用 `lumen.pair.v2`，老手机看到会落
/// [`PairingQrError::Malformed`] 而不是把新字段当老字段解。
pub const PAIRING_QR_MAGIC: &str = "lumen.pair.v1";

/// 配对码的位数（服务端 `gen_pairing_code` 产出 9 位纯数字）。
pub const PAIRING_CODE_LEN: usize = 9;

/// 账户指纹取 sha256 hex 的前多少个字符。
///
/// 16 个 hex = 64 bit。它的用途只是「这码是不是我这个账号的」——**不是**身份凭证、
/// 不承担抗碰撞之外的任何职责，64 bit 对这个用途绰绰有余，而 QR 容量是硬约束。
pub const ACCOUNT_FINGERPRINT_LEN: usize = 16;

/// 二维码载荷。
///
/// **字段名刻意用单字母**——QR 的容量随字节数上升会跳版本、模块变密、在手机上更难扫到。
/// 目标是整个 JSON < 180 字节（[`tests::典型载荷不超过容量目标`] 钉着）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingQrPayload {
    /// 固定为 [`PAIRING_QR_MAGIC`]。
    pub m: String,
    /// 规范化服务端 origin（与桌面端 `cloud::canonical_server_origin` 同款输出）。
    pub o: String,
    /// 账户指纹，见 [`account_fingerprint`]。**不放明文 `user_id`**——二维码会被拍照、
    /// 会出现在录屏和肩窥范围里，而 `user_id` 是账户的稳定标识。
    pub u: String,
    /// 被控端 device_id。
    pub t: String,
    /// 9 位配对码（服务端产出，原样透传）。
    pub c: String,
    /// 预计过期 Unix 秒。
    ///
    /// ⚠ **只做软提示，永不硬拒**：服务端的 TTL 判定才是权威，而 PC 与手机的时钟偏差
    /// 不得造成可用性故障（「码明明还在屏幕上，手机却说过期了」是最难排查的一类投诉）。
    /// [`PairingQrPayload::validate`] **不看这个字段**。
    pub e: i64,
}

/// 手机端扫到码之后的四重校验结果。
///
/// **每一种拒绝都必须有不同的 UI 文案**——把它们合并成一句「二维码无效」，
/// 用户就永远不知道自己是扫错了设备、扫了别人的码，还是遇到了钓鱼。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PairingQrError {
    /// 魔数不对 / 配对码不是 9 位纯数字。「这不是 Lumen 的配对二维码」。
    Malformed,
    /// origin 与本机登录的服务器不符。
    ///
    /// **这是钓鱼信号**，UI 要显示安全警告，且**绝不提供「是否切换服务器」的选项**——
    /// 提供了它，整条校验就变成了一个「点确定即可绕过」的对话框。
    ForeignServer,
    /// 账户指纹不符。「这是别人的配对码」。
    ForeignAccount,
    /// 目标设备与本次要连接的不符。「与你要连接的设备不符」。
    WrongTarget,
}

impl PairingQrError {
    /// 机器可读标识（golden 语料与日志用；UI 文案按它分派，不靠字符串匹配英文）。
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::Malformed => "Malformed",
            Self::ForeignServer => "ForeignServer",
            Self::ForeignAccount => "ForeignAccount",
            Self::WrongTarget => "WrongTarget",
        }
    }
}

/// 账户指纹：`sha256(user_id)` 的前 [`ACCOUNT_FINGERPRINT_LEN`] 个 **小写** hex 字符。
///
/// 两端必须用同一个实现，所以它在协议 crate 里而不是各端各写一遍——
/// 大小写、截取长度、是否带前缀，任何一处不同都会让所有扫码都判成
/// [`PairingQrError::ForeignAccount`]，而那看起来像「功能没做好」而不是「两端算法不一致」。
#[must_use]
pub fn account_fingerprint(user_id: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(user_id.as_bytes());
    let mut out = String::with_capacity(ACCOUNT_FINGERPRINT_LEN);
    for byte in digest.iter().take(ACCOUNT_FINGERPRINT_LEN.div_ceil(2)) {
        out.push(HEX[usize::from(byte >> 4)] as char);
        out.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    out.truncate(ACCOUNT_FINGERPRINT_LEN);
    out
}

impl PairingQrPayload {
    /// 按已知信息构造一个待渲染的载荷（PC 侧用）。
    #[must_use]
    pub fn new(origin: &str, user_id: &str, target: &str, code: &str, expires_at: i64) -> Self {
        Self {
            m: PAIRING_QR_MAGIC.to_string(),
            o: origin.to_string(),
            u: account_fingerprint(user_id),
            t: target.to_string(),
            c: code.to_string(),
            e: expires_at,
        }
    }

    /// 四重校验（手机端扫到码之后立刻跑）。
    ///
    /// **顺序是有讲究的**：先判「形状」（魔数、码格式）再判「身份」（服务器、账户、设备）。
    /// 形状不对说明这根本不是我们的码——可能只是随手扫到了包装盒上的二维码，
    /// 此时说「这是别人的服务器」既不准确又吓人。
    ///
    /// **不校验 [`Self::e`]**：过期由服务端判，见该字段的文档。
    ///
    /// # Errors
    /// 四种拒绝各自对应一条**不同**的 UI 文案，见 [`PairingQrError`]。
    pub fn validate(
        &self,
        expected_origin: &str,
        expected_user_fingerprint: &str,
        expected_target: &str,
    ) -> Result<(), PairingQrError> {
        if self.m != PAIRING_QR_MAGIC {
            return Err(PairingQrError::Malformed);
        }
        if self.c.len() != PAIRING_CODE_LEN || !self.c.bytes().all(|b| b.is_ascii_digit()) {
            return Err(PairingQrError::Malformed);
        }
        if self.o != expected_origin {
            return Err(PairingQrError::ForeignServer);
        }
        if self.u != expected_user_fingerprint {
            return Err(PairingQrError::ForeignAccount);
        }
        if self.t != expected_target {
            return Err(PairingQrError::WrongTarget);
        }
        Ok(())
    }

    /// 序列化成二维码里要放的那串文本。
    ///
    /// # Errors
    /// 序列化失败（理论上不会发生）时返回 serde 错误。
    pub fn to_qr_text(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// 从扫到的文本还原。
    ///
    /// # Errors
    /// 不是合法 JSON 或字段缺失时返回 serde 错误——调用方应把它转成
    /// [`PairingQrError::Malformed`]（同一句文案：「这不是 Lumen 的配对二维码」）。
    pub fn from_qr_text(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn 样例() -> PairingQrPayload {
        PairingQrPayload::new(
            "https://lumen.example.com",
            "550e8400-e29b-41d4-a716-446655440000",
            "6ba7b810-9dad-11d1-80b4-00c04fd430c8",
            "012345678",
            1_786_342_908,
        )
    }

    #[test]
    fn 账户指纹是小写hex且长度固定() {
        let fp = account_fingerprint("550e8400-e29b-41d4-a716-446655440000");
        assert_eq!(fp.len(), ACCOUNT_FINGERPRINT_LEN);
        assert!(fp
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        // 同一输入恒等；不同输入不同。
        assert_eq!(
            fp,
            account_fingerprint("550e8400-e29b-41d4-a716-446655440000")
        );
        assert_ne!(
            fp,
            account_fingerprint("550e8400-e29b-41d4-a716-446655440001")
        );
    }

    #[test]
    fn 载荷往返() {
        let p = 样例();
        let text = p.to_qr_text().expect("序列化");
        assert_eq!(PairingQrPayload::from_qr_text(&text).expect("反序列化"), p);
    }

    #[test]
    fn 典型载荷不超过容量目标() {
        // QR 容量是硬约束：字节数上去会跳版本、模块变密、在手机上更难扫到。
        // 这里用的是最长的现实取值（uuid 设备 id + 带子域的 https origin）。
        let len = 样例().to_qr_text().expect("序列化").len();
        assert!(len < 180, "载荷 {len} 字节，超过 180 字节的容量目标");
    }

    #[test]
    fn 四重校验各自命中且顺序正确() {
        let fp = account_fingerprint("550e8400-e29b-41d4-a716-446655440000");
        let target = "6ba7b810-9dad-11d1-80b4-00c04fd430c8";
        let ok = 样例();
        assert_eq!(
            ok.validate("https://lumen.example.com", &fp, target),
            Ok(())
        );

        let mut 坏魔数 = 样例();
        坏魔数.m = "lumen.pair.v2".into();
        assert_eq!(
            坏魔数.validate("https://lumen.example.com", &fp, target),
            Err(PairingQrError::Malformed)
        );

        let mut 坏码 = 样例();
        坏码.c = "12345".into();
        assert_eq!(
            坏码.validate("https://lumen.example.com", &fp, target),
            Err(PairingQrError::Malformed)
        );
        坏码.c = "01234567x".into();
        assert_eq!(
            坏码.validate("https://lumen.example.com", &fp, target),
            Err(PairingQrError::Malformed),
            "9 位但不全是数字同样不合法"
        );

        assert_eq!(
            样例().validate("https://evil.example.com", &fp, target),
            Err(PairingQrError::ForeignServer)
        );
        assert_eq!(
            样例().validate("https://lumen.example.com", "0000000000000000", target),
            Err(PairingQrError::ForeignAccount)
        );
        assert_eq!(
            样例().validate("https://lumen.example.com", &fp, "别的设备"),
            Err(PairingQrError::WrongTarget)
        );
    }

    #[test]
    fn 形状先于身份判定() {
        // 一张随手扫到的、根本不是 Lumen 的二维码，不该被说成「这是别人的服务器」。
        let mut p = 样例();
        p.m = "something.else".into();
        p.o = "https://evil.example.com".into();
        assert_eq!(
            p.validate("https://lumen.example.com", "0000000000000000", "别的设备"),
            Err(PairingQrError::Malformed),
            "魔数不对时应当先报格式问题，而不是报三条身份不符里的任意一条"
        );
    }

    #[test]
    fn 过期时刻不参与校验() {
        // 服务端的 TTL 才是权威；两端时钟偏差不得造成「码在屏幕上但手机说过期」。
        let fp = account_fingerprint("550e8400-e29b-41d4-a716-446655440000");
        let mut 早就过期 = 样例();
        早就过期.e = 0;
        assert_eq!(
            早就过期.validate(
                "https://lumen.example.com",
                &fp,
                "6ba7b810-9dad-11d1-80b4-00c04fd430c8"
            ),
            Ok(())
        );
    }
}
