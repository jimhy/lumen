//! SSH 密码与私钥口令的本机凭据存储。
//!
//! Windows 走 Credential Manager（Win32 CredWrite/CredRead）；Linux/macOS
//! 走 AES-256-GCM 加密文件（`platform_file` 模块：数据目录 `ssh-secrets/`、
//! 0600 权限、密钥由 machine-id 经 Argon2id 派生不落盘）。target 可写入
//! `SshLocalBinding`，但 credential blob、target 和错误正文都不得进入 SSH
//! 云同步 DTO 或日志。

use std::fmt;
use std::str::FromStr;

use lumen_ssh::SecretString;
use rand_core::{OsRng, RngCore};

/// Windows Credential Manager 对 generic credential blob 的字节上限。
pub const MAX_SECRET_BYTES: usize = 2_560;
const TARGET_PREFIX: &str = "Lumen/SSH/";
const PROFILE_ID_PREFIX: &str = "ssh_";
const PROFILE_ID_HEX_LEN: usize = 32;
const NONCE_BYTES: usize = 16;
const NONCE_HEX_LEN: usize = NONCE_BYTES * 2;

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
/// `Lumen/SSH/<ssh_ + 32 lowercase hex>/<password|key-passphrase>/<32 lowercase hex>`。
/// 最后一段由系统随机生成，使相同 profile ID 在不同本地作用域中也不会复用凭据。
#[derive(Clone, PartialEq, Eq)]
pub struct CredentialReference {
    profile_id: String,
    slot: CredentialSlot,
    nonce: [u8; NONCE_BYTES],
}

impl CredentialReference {
    pub fn new(profile_id: &str, slot: CredentialSlot) -> Result<Self, CredentialError> {
        if !valid_profile_id(profile_id) {
            return Err(CredentialError::InvalidProfileId);
        }
        let mut nonce = [0_u8; NONCE_BYTES];
        OsRng.fill_bytes(&mut nonce);
        Ok(Self {
            profile_id: profile_id.to_owned(),
            slot,
            nonce,
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
        format!(
            "{TARGET_PREFIX}{}/{}/{}",
            self.profile_id,
            self.slot.suffix(),
            encode_nonce(&self.nonce)
        )
    }
}

impl FromStr for CredentialReference {
    type Err = CredentialError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let rest = raw
            .strip_prefix(TARGET_PREFIX)
            .ok_or(CredentialError::InvalidReference)?;
        let mut components = rest.split('/');
        let profile_id = components.next().ok_or(CredentialError::InvalidReference)?;
        let suffix = components.next().ok_or(CredentialError::InvalidReference)?;
        let nonce = components
            .next()
            .and_then(parse_nonce)
            .ok_or(CredentialError::InvalidReference)?;
        if components.next().is_some() || !valid_profile_id(profile_id) {
            return Err(CredentialError::InvalidReference);
        }
        Ok(Self {
            profile_id: profile_id.to_owned(),
            slot: CredentialSlot::parse(suffix)?,
            nonce,
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

fn encode_nonce(nonce: &[u8; NONCE_BYTES]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(NONCE_HEX_LEN);
    for byte in nonce {
        encoded.push(HEX[usize::from(byte >> 4)] as char);
        encoded.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    encoded
}

fn parse_nonce(raw: &str) -> Option<[u8; NONCE_BYTES]> {
    if raw.len() != NONCE_HEX_LEN
        || !raw
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    let mut nonce = [0_u8; NONCE_BYTES];
    for (index, pair) in raw.as_bytes().chunks_exact(2).enumerate() {
        nonce[index] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
    }
    Some(nonce)
}

const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
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
                write!(formatter, "SSH credential write failed (OS error {code})")
            }
            Self::ReadFailed(code) => {
                write!(formatter, "SSH credential read failed (OS error {code})")
            }
            Self::DeleteFailed(code) => {
                write!(formatter, "SSH credential delete failed (OS error {code})")
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

/// “先写系统凭据、再提交本机 binding”的事务错误。
///
/// `E` 由 binding 提交层决定；本类型不实现 `Debug`/`Display`，避免调用方
/// 误把更高层上下文连同凭据引用写入日志。
pub enum CredentialTransactionError<E> {
    Write(CredentialError),
    Commit {
        error: E,
        rollback_error: Option<CredentialError>,
    },
}

/// 写入新随机 target 后执行 binding 提交；提交失败时立即幂等回滚新 target。
///
/// 旧 target 必须由调用方在本函数成功后再删除，以保证任何时刻都不会出现
/// binding 已指向尚未写入的凭据，且提交失败仍保留原 binding/secret。
pub fn write_secret_with_commit<E>(
    reference: &CredentialReference,
    secret: &str,
    commit: impl FnOnce() -> Result<(), E>,
) -> Result<(), CredentialTransactionError<E>> {
    write_secret(reference, secret).map_err(CredentialTransactionError::Write)?;
    if let Err(error) = commit() {
        let rollback_error = delete_secret(reference).err();
        return Err(CredentialTransactionError::Commit {
            error,
            rollback_error,
        });
    }
    Ok(())
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod platform {
    //! unix 凭据后端：AES-256-GCM 加密文件（2026-07-28，海风哥拍板「统一
    //! 加密文件」方案——此前非 Windows 一律 Unsupported，Linux/macOS 密码
    //! 与私钥口令保存不了、正式登录死循环重弹凭据框）。
    //!
    //! - 每条凭据一个文件：`<数据目录>/ssh-secrets/<sha256(target)[..32]>.lsec`，
    //!   内容 = magic(8) + nonce(12) + AES-256-GCM(secret, aad=target)。
    //! - 加密密钥由 machine-id（Linux /etc/machine-id；macOS kern.hostuuid）
    //!   经 Argon2id 派生，进程内缓存、**永不落盘**——文件离开本机无法解密，
    //!   明文也绝不出现在磁盘。安全级别：防明文落盘/同步泄露/随手翻看；
    //!   同机同用户进程可派生同一密钥解密（与同类终端工具一致，海风哥已知悉）。
    //! - 文件损坏/magic 不符/解密失败（换机、machine-id 变更）→ 删文件按
    //!   「无凭据」处理，用户重输即可，不让坏文件把登录卡死。

    use std::io::Write as _;
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;

    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use aes_gcm::{Aes256Gcm, Key, Nonce};
    use argon2::{Algorithm, Argon2, Version};
    use rand_core::{OsRng, RngCore};
    use sha2::{Digest, Sha256};
    use zeroize::Zeroizing;

    use super::{
        validate_secret_size, CredentialError, CredentialReference, SecretString, MAX_SECRET_BYTES,
    };

    const MAGIC: &[u8; 8] = b"LSSEC01\0";
    const NONCE_LEN: usize = 12;
    const KEY_LEN: usize = 32;
    /// Argon2id 固定应用盐（非秘密——唯一性由 machine-id 提供）。
    const KDF_SALT: &[u8; 16] = b"lumen-ssh-sec-v1";
    const SECRETS_DIR_NAME: &str = "ssh-secrets";
    const FILE_SUFFIX: &str = ".lsec";

    pub(super) fn write_secret(
        reference: &CredentialReference,
        secret: &str,
    ) -> Result<(), CredentialError> {
        validate_secret_size(secret)?;
        let dir = secrets_dir()?;
        write_secret_in(&dir, reference, secret)
    }

    pub(super) fn read_secret(
        reference: &CredentialReference,
    ) -> Result<Option<SecretString>, CredentialError> {
        let dir = secrets_dir()?;
        read_secret_in(&dir, reference)
    }

    pub(super) fn delete_secret(reference: &CredentialReference) -> Result<bool, CredentialError> {
        let dir = secrets_dir()?;
        delete_secret_in(&dir, reference)
    }

    fn secrets_dir() -> Result<PathBuf, CredentialError> {
        crate::paths::data_dir()
            .map(|dir| dir.join(SECRETS_DIR_NAME))
            .ok_or(CredentialError::Unsupported)
    }

    /// 凭据文件路径：文件名只含 target 哈希，不泄露 profile 结构、天然免路径注入。
    pub(super) fn secret_path(dir: &Path, reference: &CredentialReference) -> PathBuf {
        let digest = Sha256::digest(reference.target().as_bytes());
        let mut name = String::with_capacity(32 + FILE_SUFFIX.len());
        for byte in &digest[..16] {
            name.push_str(&format!("{byte:02x}"));
        }
        name.push_str(FILE_SUFFIX);
        dir.join(name)
    }

    /// 机器绑定派生材料：优先稳定 machine-id；读不到退化为
    /// 用户名+主机名+家目录（同机同用户仍确定，绑定强度降为「本机本用户」）。
    fn machine_material() -> Vec<u8> {
        #[cfg(target_os = "linux")]
        {
            for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
                if let Ok(id) = std::fs::read_to_string(path) {
                    let id = id.trim();
                    if !id.is_empty() {
                        return id.as_bytes().to_vec();
                    }
                }
            }
        }
        #[cfg(target_os = "macos")]
        {
            if let Some(uuid) = macos_host_uuid() {
                return uuid.into_bytes();
            }
        }
        let mut fallback = std::env::var("USER").unwrap_or_default();
        fallback.push('\0');
        fallback.push_str(&hostname());
        fallback.push('\0');
        fallback.push_str(&std::env::var("HOME").unwrap_or_default());
        fallback.into_bytes()
    }

    #[cfg(target_os = "macos")]
    fn macos_host_uuid() -> Option<String> {
        // sysctlbyname("kern.hostuuid")：libSystem 直链，无需 IOKit 依赖。
        extern "C" {
            fn sysctlbyname(
                name: *const core::ffi::c_char,
                oldp: *mut core::ffi::c_void,
                oldlenp: *mut usize,
                newp: *const core::ffi::c_void,
                newlen: usize,
            ) -> core::ffi::c_int;
        }
        let mut buf = [0u8; 64];
        let mut len = buf.len();
        // SAFETY: buf 可写 len 字节；成功时 oldlenp 写入含 NUL 的实际长度。
        let rc = unsafe {
            sysctlbyname(
                c"kern.hostuuid".as_ptr(),
                buf.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null(),
                0,
            )
        };
        if rc != 0 || len < 2 {
            return None;
        }
        let end = buf[..len].iter().position(|&b| b == 0).unwrap_or(len);
        String::from_utf8(buf[..end].to_vec()).ok()
    }

    fn hostname() -> String {
        extern "C" {
            fn gethostname(name: *mut core::ffi::c_char, len: usize) -> core::ffi::c_int;
        }
        let mut buf = [0u8; 256];
        // SAFETY: buf 可写；gethostname 成功返回 0 并 NUL 结尾。
        let rc = unsafe { gethostname(buf.as_mut_ptr().cast(), buf.len()) };
        if rc != 0 {
            return String::new();
        }
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..end]).into_owned()
    }

    /// Argon2id(machine_material) → 32B AES key。进程内只算一次（~50ms），
    /// 材料与盐固定 → 同机同用户每次派生同一密钥。
    fn master_key() -> Result<Zeroizing<[u8; KEY_LEN]>, CredentialError> {
        static KEY: OnceLock<Result<Zeroizing<[u8; KEY_LEN]>, CredentialError>> = OnceLock::new();
        KEY.get_or_init(|| {
            let material = Zeroizing::new(machine_material());
            if material.is_empty() {
                return Err(CredentialError::Unsupported);
            }
            let params = argon2::Params::default();
            let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
            let mut key = Zeroizing::new([0u8; KEY_LEN]);
            argon2
                .hash_password_into(material.as_slice(), KDF_SALT, &mut key[..])
                .map_err(|_| CredentialError::Unsupported)?;
            Ok(key)
        })
        .clone()
    }

    fn cipher() -> Result<Aes256Gcm, CredentialError> {
        let key = master_key()?;
        Ok(Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key.as_slice())))
    }

    fn io_code(error: &std::io::Error) -> u32 {
        error.raw_os_error().unwrap_or(-1).unsigned_abs()
    }

    pub(super) fn write_secret_in(
        dir: &Path,
        reference: &CredentialReference,
        secret: &str,
    ) -> Result<(), CredentialError> {
        use std::os::unix::fs::OpenOptionsExt as _;

        let cipher = cipher()?;
        let mut nonce = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce);
        let ciphertext = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: secret.as_bytes(),
                    aad: reference.target().as_bytes(),
                },
            )
            .map_err(|_| CredentialError::WriteFailed(0))?;
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        builder
            .create(dir)
            .map_err(|e| CredentialError::WriteFailed(io_code(&e)))?;

        let path = secret_path(dir, reference);
        let tmp = path.with_extension("lsec.tmp");
        let write_tmp = || -> Result<(), std::io::Error> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            file.write_all(MAGIC)?;
            file.write_all(&nonce)?;
            file.write_all(&ciphertext)?;
            file.sync_all()
        };
        let result = write_tmp()
            .map_err(|e| CredentialError::WriteFailed(io_code(&e)))
            .and_then(|()| {
                std::fs::rename(&tmp, &path).map_err(|e| CredentialError::WriteFailed(io_code(&e)))
            });
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    pub(super) fn read_secret_in(
        dir: &Path,
        reference: &CredentialReference,
    ) -> Result<Option<SecretString>, CredentialError> {
        let path = secret_path(dir, reference);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(CredentialError::ReadFailed(io_code(&e))),
        };
        let parsed = parse_secret_file(&bytes, reference);
        match parsed {
            Ok(secret) => Ok(Some(secret)),
            Err(()) => {
                // 损坏/换机/被调换的文件：删除自愈，按「无凭据」处理（见模块注释）。
                let _ = std::fs::remove_file(&path);
                Ok(None)
            }
        }
    }

    fn parse_secret_file(
        bytes: &[u8],
        reference: &CredentialReference,
    ) -> Result<SecretString, ()> {
        if bytes.len() < MAGIC.len() + NONCE_LEN + 16 || bytes.len() > MAGIC.len() + NONCE_LEN + MAX_SECRET_BYTES + 16 {
            return Err(());
        }
        if &bytes[..MAGIC.len()] != MAGIC {
            return Err(());
        }
        let cipher = cipher().map_err(|_| ())?;
        let plaintext = cipher
            .decrypt(
                Nonce::from_slice(&bytes[MAGIC.len()..MAGIC.len() + NONCE_LEN]),
                Payload {
                    msg: &bytes[MAGIC.len() + NONCE_LEN..],
                    aad: reference.target().as_bytes(),
                },
            )
            .map_err(|_| ())?;
        let text = String::from_utf8(plaintext).map_err(|_| ())?;
        Ok(SecretString::new(text))
    }

    pub(super) fn delete_secret_in(
        dir: &Path,
        reference: &CredentialReference,
    ) -> Result<bool, CredentialError> {
        let path = secret_path(dir, reference);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(CredentialError::DeleteFailed(io_code(&e))),
        }
    }

    #[cfg(test)]
    pub(super) mod test_support {
        //! 测试后门：带目录参数的后端入口与路径计算，避免污染真实数据目录。
        //!
        //! **可见性要两头同时够，缺一头就编译不过**，而且两头都只在 Linux/macOS 上
        //! 才暴露——Windows 上整个 `platform` 块与对应测试都被 cfg 掉，本地测试
        //! 全绿察觉不到，所以这处自 v1.0.22 起一直红在 CI 的 test-linux / test-macos 上：
        //!
        //! - **`use` 这一头**：真正的使用者是上一层的 `credentials::tests`
        //!   （`use super::platform::test_support::*`）。本模块内 `super` 指的是
        //!   `platform`，写 `pub(super)` 只能让名字对 `platform` 自己可见，使用者会报
        //!   四个 E0425「cannot find function」。故这里精确限定到 `credentials`。
        //! - **被 re-export 的那四个 `fn` 那一头**：私有项不能被 re-export 成更高的
        //!   可见性（E0364）。只提 `use` 不提函数，E0425 就换成 E0364，照红不误
        //!   （v1.1.1 发版时 CI 就卡在这一步）。它们在 `platform` 里写 `pub(super)`
        //!   正好等于 `credentials`，与本 `use` 的范围齐平——这也是同文件
        //!   `write_secret` / `read_secret` / `delete_secret` 一贯的写法。
        //!
        //! 两头都精确停在 `credentials`，加上 `platform` 本身是私有 mod，后门不会
        //! 外泄：越过 `credentials` 去引用会 E0603。
        pub(in crate::ssh::credentials) use super::{
            delete_secret_in, read_secret_in, secret_path, write_secret_in,
        };
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
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
        let password_target = password.target();
        assert!(
            password_target.starts_with("Lumen/SSH/ssh_0123456789abcdef0123456789abcdef/password/")
        );
        assert_eq!(
            password_target.len(),
            TARGET_PREFIX.len() + PROFILE_ID.len() + 1 + 8 + 1 + 32
        );
        assert_eq!(
            password_target.parse::<CredentialReference>().unwrap(),
            password
        );
        let passphrase = CredentialReference::key_passphrase(PROFILE_ID).unwrap();
        assert!(passphrase
            .target()
            .starts_with("Lumen/SSH/ssh_0123456789abcdef0123456789abcdef/key-passphrase/"));
        assert_ne!(
            password.target(),
            CredentialReference::password(PROFILE_ID).unwrap().target()
        );

        for invalid in [
            "",
            "Lumen/SSH/ssh_0123456789abcdef0123456789abcde/password/00000000000000000000000000000000",
            "Lumen/SSH/ssh_0123456789ABCDEF0123456789ABCDEF/password/00000000000000000000000000000000",
            "Lumen/SSH/ssh_0123456789abcdef0123456789abcdef/Password/00000000000000000000000000000000",
            "Lumen/SSH/ssh_0123456789abcdef0123456789abcdef/key_passphrase/00000000000000000000000000000000",
            "Lumen/SSH/ssh_0123456789abcdef0123456789abcdef/password",
            "Lumen/SSH/ssh_0123456789abcdef0123456789abcdef/password/0000000000000000000000000000000",
            "Lumen/SSH/ssh_0123456789abcdef0123456789abcdef/password/0000000000000000000000000000000A",
            "Lumen/SSH/ssh_0123456789abcdef0123456789abcdef/password/00000000000000000000000000000000/extra",
            "lumen/SSH/ssh_0123456789abcdef0123456789abcdef/password/00000000000000000000000000000000",
            "Lumen/SSH/grp_0123456789abcdef0123456789abcdef/password/00000000000000000000000000000000",
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

    // 这条守的是**无原生后端**那个 fallback `platform` 的契约（文件末尾那份
    // `cfg(not(any(windows, linux, macos)))` 实现，三个操作一律 Unsupported），
    // 所以 cfg 必须与它逐字对齐。
    //
    // 原先写的是 `cfg(not(windows))`：2026-07-28 的 6a2178b 给 Linux/macOS 换上了
    // 真正的 AES-GCM 加密文件后端、它们不再返回 Unsupported，却没同步收窄这里的
    // cfg，于是这条测试在 unix 上必然失败。它被同文件测试后门的 18 个编译错误挡了
    // 20 天没人看见——编译一修好就立刻露出来。
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    #[test]
    fn 无原生后端的平台操作统一返回unsupported() {
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

    #[cfg(windows)]
    #[test]
    fn binding提交失败会回滚刚写入的随机target() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            ^ (u128::from(std::process::id()) << 32);
        let profile_id = format!("ssh_{nonce:032x}");
        let reference = CredentialReference::password(&profile_id).unwrap();
        let _ = delete_secret(&reference);

        let result = write_secret_with_commit(
            &reference,
            "transaction-rollback-secret",
            || Err::<(), _>("binding commit failed"),
        );
        match result {
            Err(CredentialTransactionError::Commit {
                error,
                rollback_error,
            }) => {
                assert_eq!(error, "binding commit failed");
                assert!(rollback_error.is_none());
            }
            Err(CredentialTransactionError::Write(_)) => {
                panic!("Credential Manager write unexpectedly failed")
            }
            Ok(()) => panic!("failing binding commit unexpectedly succeeded"),
        }
        assert!(read_secret(&reference).unwrap().is_none());
    }

    // ── unix（Linux/macOS）AES-GCM 加密文件后端 ────────────────────────────

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn unix_temp_dir(tag: &str) -> std::path::PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("lumen-cred-{tag}-{}-{nonce}", std::process::id()))
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn unix加密文件_往返且删除幂等() {
        use super::platform::test_support::*;
        let dir = unix_temp_dir("roundtrip");
        let reference = CredentialReference::password(PROFILE_ID).unwrap();

        write_secret_in(&dir, &reference, "s3cr3t-密码").unwrap();
        let loaded = read_secret_in(&dir, &reference)
            .unwrap()
            .expect("写入后应能读回");
        assert!(!loaded.is_empty());
        // 文件权限必须是 0600（仅属主可读写）。
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let permissions = std::fs::metadata(secret_path(&dir, &reference))
                .unwrap()
                .permissions();
            assert_eq!(permissions.mode() & 0o777, 0o600);
        }
        assert!(delete_secret_in(&dir, &reference).unwrap());
        assert!(read_secret_in(&dir, &reference).unwrap().is_none());
        assert!(!delete_secret_in(&dir, &reference).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn unix加密文件_损坏文件自愈删除() {
        use super::platform::test_support::*;
        let dir = unix_temp_dir("corrupt");
        let reference = CredentialReference::password(PROFILE_ID).unwrap();
        let path = secret_path(&dir, &reference);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, b"garbage-not-our-format").unwrap();

        // 损坏文件按「无凭据」处理并删除，不让登录卡死。
        assert!(read_secret_in(&dir, &reference).unwrap().is_none());
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn unix加密文件_调换防护() {
        use super::platform::test_support::*;
        let dir = unix_temp_dir("swap");
        let ref_a = CredentialReference::password(PROFILE_ID).unwrap();
        let other_id = "ssh_abcdef0123456789abcdef0123456789";
        let ref_b = CredentialReference::password(other_id).unwrap();

        write_secret_in(&dir, &ref_a, "secret-a").unwrap();
        write_secret_in(&dir, &ref_b, "secret-b").unwrap();
        // 攻击/事故：把 A 的密文覆盖到 B 的文件上——AAD 含 target，解密必失败。
        std::fs::copy(secret_path(&dir, &ref_a), secret_path(&dir, &ref_b)).unwrap();
        assert!(read_secret_in(&dir, &ref_b).unwrap().is_none());
        // A 自己仍完好可读。
        assert!(read_secret_in(&dir, &ref_a).unwrap().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
