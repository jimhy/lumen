//! SSH 密码与私钥口令的本机凭据存储。
//!
//! 本模块只处理 Windows Credential Manager；target 可写入 `SshLocalBinding`，但
//! credential blob、target 和错误正文都不得进入 SSH 云同步 DTO 或日志。

use std::fmt;
use std::str::FromStr;

use lumen_ssh::SecretString;

/// Windows Credential Manager 对 generic credential blob 的字节上限。
pub const MAX_SECRET_BYTES: usize = 2_560;
const TARGET_PREFIX: &str = "Lumen/SSH/";
const PROFILE_ID_PREFIX: &str = "ssh_";
const PROFILE_ID_HEX_LEN: usize = 32;

/// 一条 SSH 本机凭据的用途。它只决定固定 target 的末段，不承载秘密。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSlot {
    Password,
    KeyPassphrase,
}

impl CredentialSlot {
    const fn suffix(self) -> &'static str {
        match self {
            Self::Password => "password",
            Self::KeyPassphrase => "key-passphrase",
        }
    }

    fn parse(raw: &str) -> Result<Self, CredentialError> {
        match raw {
            "password" => Ok(Self::Password),
            "key-passphrase" => Ok(Self::KeyPassphrase),
            _ => Err(CredentialError::InvalidReference),
        }
    }
}

/// 已严格校验的 Credential Manager 引用。
///
/// target 始终为
/// `Lumen/SSH/<ssh_ + 32 lowercase hex>/<password|key-passphrase>`。
#[derive(Clone, PartialEq, Eq)]
pub struct CredentialReference {
    profile_id: String,
    slot: CredentialSlot,
}

impl CredentialReference {
    pub fn new(profile_id: &str, slot: CredentialSlot) -> Result<Self, CredentialError> {
        if !valid_profile_id(profile_id) {
            return Err(CredentialError::InvalidProfileId);
        }
        Ok(Self {
            profile_id: profile_id.to_owned(),
            slot,
        })
    }

    pub fn password(profile_id: &str) -> Result<Self, CredentialError> {
        Self::new(profile_id, CredentialSlot::Password)
    }

    pub fn key_passphrase(profile_id: &str) -> Result<Self, CredentialError> {
        Self::new(profile_id, CredentialSlot::KeyPassphrase)
    }

    pub fn profile_id(&self) -> &str {
        &self.profile_id
    }

    pub const fn slot(&self) -> CredentialSlot {
        self.slot
    }

    pub fn target(&self) -> String {
        format!("{TARGET_PREFIX}{}/{}", self.profile_id, self.slot.suffix())
    }
}

impl FromStr for CredentialReference {
    type Err = CredentialError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let rest = raw
            .strip_prefix(TARGET_PREFIX)
            .ok_or(CredentialError::InvalidReference)?;
        let (profile_id, suffix) = rest
            .split_once('/')
            .ok_or(CredentialError::InvalidReference)?;
        if suffix.contains('/') || !valid_profile_id(profile_id) {
            return Err(CredentialError::InvalidReference);
        }
        Ok(Self {
            profile_id: profile_id.to_owned(),
            slot: CredentialSlot::parse(suffix)?,
        })
    }
}

impl fmt::Display for CredentialReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.target())
    }
}

impl fmt::Debug for CredentialReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CredentialReference")
            .field("profile_id", &"<validated>")
            .field("slot", &self.slot)
            .finish()
    }
}

/// 凭据存储错误。变体只保留操作类别与 Win32 code，不保存 target、秘密或系统错误正文。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialError {
    InvalidProfileId,
    InvalidReference,
    SecretTooLarge,
    InvalidStoredSecret,
    CorruptStoredCredential,
    Unsupported,
    WriteFailed(u32),
    ReadFailed(u32),
    DeleteFailed(u32),
}

impl fmt::Display for CredentialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidProfileId => formatter.write_str("invalid SSH profile id"),
            Self::InvalidReference => formatter.write_str("invalid SSH credential reference"),
            Self::SecretTooLarge => {
                write!(formatter, "SSH credential exceeds {MAX_SECRET_BYTES} bytes")
            }
            Self::InvalidStoredSecret => {
                formatter.write_str("stored SSH credential is not valid UTF-8")
            }
            Self::CorruptStoredCredential => {
                formatter.write_str("stored SSH credential is corrupt")
            }
            Self::Unsupported => {
                formatter.write_str("SSH credential storage is unsupported on this platform")
            }
            Self::WriteFailed(code) => {
                write!(formatter, "SSH credential write failed (Win32={code})")
            }
            Self::ReadFailed(code) => {
                write!(formatter, "SSH credential read failed (Win32={code})")
            }
            Self::DeleteFailed(code) => {
                write!(formatter, "SSH credential delete failed (Win32={code})")
            }
        }
    }
}

impl std::error::Error for CredentialError {}

/// 写入或覆盖一条本机凭据。
///
/// 调用方仍应在调用后立即清空 UI 中的明文 `String`；本函数创建的内部字节副本会在
/// Win32 调用返回后自动 zeroize。
pub fn write_secret(reference: &CredentialReference, secret: &str) -> Result<(), CredentialError> {
    platform::write_secret(reference, secret)
}

/// 读取本机凭据。不存在时返回 `Ok(None)`。
pub fn read_secret(
    reference: &CredentialReference,
) -> Result<Option<SecretString>, CredentialError> {
    platform::read_secret(reference)
}

/// 删除本机凭据。不存在时返回 `Ok(false)`，重复删除安全幂等。
pub fn delete_secret(reference: &CredentialReference) -> Result<bool, CredentialError> {
    platform::delete_secret(reference)
}

fn valid_profile_id(profile_id: &str) -> bool {
    profile_id.len() == PROFILE_ID_PREFIX.len() + PROFILE_ID_HEX_LEN
        && profile_id.starts_with(PROFILE_ID_PREFIX)
        && profile_id[PROFILE_ID_PREFIX.len()..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_secret_size(secret: &str) -> Result<(), CredentialError> {
    if secret.len() > MAX_SECRET_BYTES {
        Err(CredentialError::SecretTooLarge)
    } else {
        Ok(())
    }
}

#[cfg(windows)]
mod platform {
    use std::ptr;

    use windows_sys::Win32::Foundation::{GetLastError, ERROR_NOT_FOUND};
    use windows_sys::Win32::Security::Credentials::{
        CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE,
        CRED_TYPE_GENERIC,
    };
    use zeroize::{Zeroize, Zeroizing};

    use super::{
        validate_secret_size, CredentialError, CredentialReference, SecretString, MAX_SECRET_BYTES,
    };

    struct CredentialBuffer(*mut CREDENTIALW);

    impl Drop for CredentialBuffer {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: CredReadW 成功返回的 CredentialBlob 对声明的字节数可写；
                // 在 CredFree 前用 zeroize 的 volatile 实现清除 OS 分配缓冲区。
                let credential = unsafe { &mut *self.0 };
                if credential.CredentialBlobSize != 0 && !credential.CredentialBlob.is_null() {
                    let blob = unsafe {
                        std::slice::from_raw_parts_mut(
                            credential.CredentialBlob,
                            credential.CredentialBlobSize as usize,
                        )
                    };
                    blob.zeroize();
                }
                // SAFETY: CredReadW 成功后返回的缓冲区必须且只需用 CredFree 释放。
                unsafe { CredFree(self.0.cast::<core::ffi::c_void>()) };
            }
        }
    }

    pub(super) fn write_secret(
        reference: &CredentialReference,
        secret: &str,
    ) -> Result<(), CredentialError> {
        validate_secret_size(secret)?;
        let mut target = wide_zeroized(&reference.target());
        let mut blob = Zeroizing::new(secret.as_bytes().to_vec());
        let blob_pointer = if blob.is_empty() {
            ptr::null_mut()
        } else {
            blob.as_mut_ptr()
        };
        let mut credential = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: target.as_mut_ptr(),
            CredentialBlobSize: blob.len() as u32,
            CredentialBlob: blob_pointer,
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            ..Default::default()
        };
        // SAFETY: credential 的 target 是 NUL 结尾 UTF-16；blob 指针在调用期间有效，
        // 大小已限制为 Credential Manager 上限；其余可选字段为 null/0。
        if unsafe { CredWriteW(&raw mut credential, 0) } == 0 {
            // SAFETY: 紧跟失败的 CredWriteW 读取线程 last-error。
            let code = unsafe { GetLastError() };
            return Err(CredentialError::WriteFailed(code));
        }
        Ok(())
    }

    pub(super) fn read_secret(
        reference: &CredentialReference,
    ) -> Result<Option<SecretString>, CredentialError> {
        let target = wide_zeroized(&reference.target());
        let mut raw = ptr::null_mut::<CREDENTIALW>();
        // SAFETY: target 是 NUL 结尾 UTF-16；raw 是有效 out-pointer。
        if unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &raw mut raw) } == 0 {
            // SAFETY: 紧跟失败的 CredReadW 读取线程 last-error。
            let code = unsafe { GetLastError() };
            return if code == ERROR_NOT_FOUND {
                Ok(None)
            } else {
                Err(CredentialError::ReadFailed(code))
            };
        }
        let buffer = CredentialBuffer(raw);
        if buffer.0.is_null() {
            return Err(CredentialError::CorruptStoredCredential);
        }
        // SAFETY: CredReadW 成功且指针非空，CredentialBuffer 保证读取期间不释放。
        let credential = unsafe { &*buffer.0 };
        let blob_len = credential.CredentialBlobSize as usize;
        if blob_len > MAX_SECRET_BYTES || (blob_len != 0 && credential.CredentialBlob.is_null()) {
            return Err(CredentialError::CorruptStoredCredential);
        }
        let mut blob = if blob_len == 0 {
            Zeroizing::new(Vec::new())
        } else {
            // SAFETY: Win32 返回的 blob 指针对 CredentialBlobSize 字节有效，且该大小
            // 已先限制到 2560；立即复制到受 zeroize 保护的本进程缓冲区。
            let bytes = unsafe { std::slice::from_raw_parts(credential.CredentialBlob, blob_len) };
            Zeroizing::new(bytes.to_vec())
        };
        let value = std::str::from_utf8(&blob)
            .map_err(|_| CredentialError::InvalidStoredSecret)?
            .to_owned();
        // 尽早清除副本，不等待函数结束；Win32 原缓冲由 CredentialBuffer::drop 释放。
        blob.zeroize();
        Ok(Some(SecretString::new(value)))
    }

    pub(super) fn delete_secret(reference: &CredentialReference) -> Result<bool, CredentialError> {
        let target = wide_zeroized(&reference.target());
        // SAFETY: target 是 NUL 结尾 UTF-16，type/flags 符合 generic credential API。
        if unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) } != 0 {
            return Ok(true);
        }
        // SAFETY: 紧跟失败的 CredDeleteW 读取线程 last-error。
        let code = unsafe { GetLastError() };
        if code == ERROR_NOT_FOUND {
            Ok(false)
        } else {
            Err(CredentialError::DeleteFailed(code))
        }
    }

    fn wide_zeroized(value: &str) -> Zeroizing<Vec<u16>> {
        Zeroizing::new(value.encode_utf16().chain(std::iter::once(0)).collect())
    }
}

#[cfg(not(windows))]
mod platform {
    use super::{CredentialError, CredentialReference, SecretString};

    pub(super) fn write_secret(
        _reference: &CredentialReference,
        _secret: &str,
    ) -> Result<(), CredentialError> {
        Err(CredentialError::Unsupported)
    }

    pub(super) fn read_secret(
        _reference: &CredentialReference,
    ) -> Result<Option<SecretString>, CredentialError> {
        Err(CredentialError::Unsupported)
    }

    pub(super) fn delete_secret(_reference: &CredentialReference) -> Result<bool, CredentialError> {
        Err(CredentialError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROFILE_ID: &str = "ssh_0123456789abcdef0123456789abcdef";

    #[test]
    fn 引用严格解析且target格式固定() {
        let password = CredentialReference::password(PROFILE_ID).unwrap();
        assert_eq!(
            password.target(),
            "Lumen/SSH/ssh_0123456789abcdef0123456789abcdef/password"
        );
        assert_eq!(
            password.target().parse::<CredentialReference>().unwrap(),
            password
        );
        let passphrase = CredentialReference::key_passphrase(PROFILE_ID).unwrap();
        assert_eq!(
            passphrase.target(),
            "Lumen/SSH/ssh_0123456789abcdef0123456789abcdef/key-passphrase"
        );

        for invalid in [
            "",
            "Lumen/SSH/ssh_0123456789abcdef0123456789abcde/password",
            "Lumen/SSH/ssh_0123456789ABCDEF0123456789ABCDEF/password",
            "Lumen/SSH/ssh_0123456789abcdef0123456789abcdef/Password",
            "Lumen/SSH/ssh_0123456789abcdef0123456789abcdef/key_passphrase",
            "Lumen/SSH/ssh_0123456789abcdef0123456789abcdef/password/extra",
            "lumen/SSH/ssh_0123456789abcdef0123456789abcdef/password",
            "Lumen/SSH/grp_0123456789abcdef0123456789abcdef/password",
        ] {
            assert!(invalid.parse::<CredentialReference>().is_err(), "{invalid}");
        }
    }

    #[test]
    fn 错误debug和display不回显target或秘密() {
        let target_sentinel = "TARGET_SENTINEL_SHOULD_NOT_APPEAR";
        let secret_sentinel = "SECRET_SENTINEL_SHOULD_NOT_APPEAR";
        let parse_error = format!("{TARGET_PREFIX}{target_sentinel}/password")
            .parse::<CredentialReference>()
            .unwrap_err();
        let oversized_secret = format!("{secret_sentinel}{}", "x".repeat(MAX_SECRET_BYTES));
        let size_error = validate_secret_size(&oversized_secret).unwrap_err();
        for error in [
            parse_error,
            size_error,
            CredentialError::InvalidStoredSecret,
            CredentialError::WriteFailed(5),
        ] {
            let rendered = format!("{error:?} {error}");
            assert!(!rendered.contains(target_sentinel));
            assert!(!rendered.contains(secret_sentinel));
            assert!(!rendered.contains(PROFILE_ID));
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn 非windows操作统一返回unsupported() {
        let reference = CredentialReference::password(PROFILE_ID).unwrap();
        assert_eq!(
            write_secret(&reference, "secret"),
            Err(CredentialError::Unsupported)
        );
        assert!(matches!(
            read_secret(&reference),
            Err(CredentialError::Unsupported)
        ));
        assert_eq!(delete_secret(&reference), Err(CredentialError::Unsupported));
    }

    #[cfg(windows)]
    #[test]
    fn windows_credential_manager可清理往返且删除幂等() {
        use std::time::{SystemTime, UNIX_EPOCH};

        struct Cleanup(CredentialReference);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = delete_secret(&self.0);
            }
        }

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            ^ u128::from(std::process::id());
        let profile_id = format!("ssh_{nonce:032x}");
        let reference = CredentialReference::password(&profile_id).unwrap();
        let cleanup = Cleanup(reference.clone());
        let _ = delete_secret(&reference);

        write_secret(&reference, "credential-roundtrip-secret").unwrap();
        let loaded = read_secret(&reference).unwrap().expect("credential exists");
        assert!(!loaded.is_empty());
        assert!(delete_secret(&reference).unwrap());
        assert!(read_secret(&reference).unwrap().is_none());
        assert!(!delete_secret(&reference).unwrap());
        drop(cleanup);
    }
}
