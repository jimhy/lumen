use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use super::model::ProfileId;

const MAX_SERVER_ORIGIN_BYTES: usize = 2_048;

/// 已严格校验的账号存储作用域。字段私有，外部只能经
/// [`StorageScope::account`] 构造，不能绕过 origin/账号规范校验。
#[derive(Clone, PartialEq, Eq)]
pub struct AccountStorageScope {
    server_origin: String,
    account_id: String,
    server_key: String,
}

/// SSH 库存的本地/账号作用域。账号缓存同时由服务端 origin 与账号 UUID 隔离。
#[derive(Clone, PartialEq, Eq)]
pub enum StorageScope {
    Local,
    Account(AccountStorageScope),
}

impl StorageScope {
    /// 构造账号作用域。
    ///
    /// `server_origin` 必须已经是规范化的 HTTP(S) 根 origin，例如
    /// `https://lumen.example` 或 `http://127.0.0.1:8787`。这里故意不接受
    /// bare 地址，也不会静默规整大小写、默认端口或尾斜杠，避免同一服务被映射
    /// 到多个目录。
    pub fn account(server_origin: &str, account_id: &str) -> Result<Self, &'static str> {
        let server_origin = canonical_server_origin(server_origin)?;
        let account_id = canonical_account_id(account_id)?;
        let server_key = sha256_hex(server_origin.as_bytes());
        Ok(Self::Account(AccountStorageScope {
            server_origin,
            account_id,
            server_key,
        }))
    }

    pub fn directory(&self, data_root: &Path) -> Result<PathBuf, &'static str> {
        let ssh_root = data_root.join("ssh");
        match self {
            Self::Local => Ok(ssh_root.join("unclaimed")),
            Self::Account(scope) => Ok(ssh_root
                .join("servers")
                .join(format!("srv_{}", scope.server_key))
                .join("accounts")
                .join(format!("acct_{}", scope.account_id.replace('-', "")))),
        }
    }

    pub fn canonical_account_id(&self) -> Result<Option<String>, &'static str> {
        match self {
            Self::Local => Ok(None),
            Self::Account(scope) => Ok(Some(scope.account_id.clone())),
        }
    }

    pub fn server_origin(&self) -> Option<&str> {
        match self {
            Self::Local => None,
            Self::Account(scope) => Some(&scope.server_origin),
        }
    }

    pub(crate) fn account_identity(&self) -> Option<(&str, &str)> {
        match self {
            Self::Local => None,
            Self::Account(scope) => Some((&scope.server_origin, &scope.account_id)),
        }
    }
}

/// 只保留在本机的认证材料绑定。这里存的是系统凭据库引用，不是密码明文。
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshLocalBinding {
    pub profile_id: ProfileId,
    pub private_key_path: Option<PathBuf>,
    pub password_credential_ref: Option<String>,
    pub key_passphrase_credential_ref: Option<String>,
}

impl fmt::Debug for SshLocalBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SshLocalBinding")
            .field("profile_id", &self.profile_id)
            .field(
                "private_key_path",
                &self.private_key_path.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "password_credential_ref",
                &self.password_credential_ref.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "key_passphrase_credential_ref",
                &self
                    .key_passphrase_credential_ref
                    .as_ref()
                    .map(|_| "<redacted>"),
            )
            .finish()
    }
}

fn canonical_account_id(raw: &str) -> Result<String, &'static str> {
    if raw.is_empty() {
        return Err("账号 ID 不能为空");
    }
    if raw.trim() != raw || raw.len() != 36 {
        return Err("账号 ID 必须是 canonical UUID");
    }
    for (index, byte) in raw.bytes().enumerate() {
        let is_hyphen = matches!(index, 8 | 13 | 18 | 23);
        if (is_hyphen && byte != b'-') || (!is_hyphen && !byte.is_ascii_hexdigit()) {
            return Err("账号 ID 必须是 canonical UUID");
        }
    }
    Ok(raw.to_ascii_lowercase())
}

fn canonical_server_origin(raw: &str) -> Result<String, &'static str> {
    if raw.is_empty()
        || raw.len() > MAX_SERVER_ORIGIN_BYTES
        || raw.trim() != raw
        || raw.chars().any(char::is_control)
        || raw.chars().any(char::is_whitespace)
    {
        return Err("服务端 origin 为空、过长或含空白/控制字符");
    }

    let parsed = Url::parse(raw).map_err(|_| "服务端 origin URL 非法")?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("服务端 origin 只支持 http/https");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("服务端 origin 不能包含用户信息");
    }
    if parsed.path() != "/" || parsed.query().is_some() || parsed.fragment().is_some() {
        return Err("服务端 origin 不能包含路径、查询或片段");
    }
    if parsed.host_str().is_some_and(|host| host.ends_with('.')) {
        return Err("服务端 origin 主机名不能带尾点");
    }

    let canonical = parsed.origin().ascii_serialization();
    if canonical == "null" || canonical != raw {
        return Err("服务端 origin 必须是已规范化的根 origin");
    }
    Ok(canonical)
}

fn sha256_hex(input: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(input);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 账号id必须是canonical_uuid且目录组件固定长度() {
        let root = Path::new(r"C:\LumenData");
        let account_a = StorageScope::account(
            "https://lumen.example",
            "550E8400-E29B-41D4-A716-446655440000",
        )
        .unwrap()
        .directory(root)
        .unwrap();
        let account_b = StorageScope::account(
            "https://lumen.example",
            "550e8400-e29b-41d4-a716-446655440001",
        )
        .unwrap()
        .directory(root)
        .unwrap();

        assert!(account_a.starts_with(root.join("ssh").join("servers")));
        assert_eq!(account_a.parent(), account_b.parent());
        assert_ne!(account_a, account_b);
        assert_eq!(
            account_a.file_name().unwrap().to_string_lossy().len(),
            "acct_".len() + 32
        );
        for invalid in [
            "",
            " 550e8400-e29b-41d4-a716-446655440000",
            "../../other",
            "550e8400e29b41d4a716446655440000",
            "550e8400-e29b-41d4-a716-44665544000/",
        ] {
            assert!(StorageScope::account("https://lumen.example", invalid).is_err());
        }
    }

    #[test]
    fn origin必须是严格规范的http根origin() {
        for valid in [
            "https://lumen.example",
            "http://127.0.0.1:8787",
            "http://[::1]:8787",
        ] {
            let scope =
                StorageScope::account(valid, "550e8400-e29b-41d4-a716-446655440000").unwrap();
            assert_eq!(scope.server_origin(), Some(valid));
        }
        for invalid in [
            "",
            "lumen.example:8787",
            "HTTPS://lumen.example",
            "https://LUMEN.example",
            "https://lumen.example/",
            "https://lumen.example:443",
            "https://user@lumen.example",
            "https://lumen.example/api",
            "https://lumen.example?q=1",
            "https://lumen.example#fragment",
            "ftp://lumen.example",
            "https://lumen.example.",
        ] {
            assert!(
                StorageScope::account(invalid, "550e8400-e29b-41d4-a716-446655440000").is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn 相同账号在不同origin有不同sha256目录() {
        let root = Path::new(r"D:\data");
        let account_id = "550e8400-e29b-41d4-a716-446655440000";
        let first = StorageScope::account("https://one.example", account_id)
            .unwrap()
            .directory(root)
            .unwrap();
        let second = StorageScope::account("https://two.example", account_id)
            .unwrap()
            .directory(root)
            .unwrap();
        assert_ne!(first, second);
        let server_directory = first.parent().unwrap().parent().unwrap();
        let server_component = server_directory.file_name().unwrap().to_string_lossy();
        assert_eq!(server_component.len(), "srv_".len() + 64);
        assert!(server_component["srv_".len()..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
    }

    #[test]
    fn 本地作用域与账号作用域目录隔离() {
        let root = Path::new(r"D:\data");
        let local = StorageScope::Local.directory(root).unwrap();
        let account = StorageScope::account(
            "https://lumen.example",
            "550e8400-e29b-41d4-a716-446655440000",
        )
        .unwrap()
        .directory(root)
        .unwrap();
        assert_ne!(local, account);
        assert_eq!(local, root.join("ssh").join("unclaimed"));
    }

    #[test]
    fn sha256目录键符合标准向量() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn 本地绑定debug不泄漏路径和凭据引用() {
        let binding = SshLocalBinding {
            profile_id: "ssh_00000000000000000000000000000000".to_owned(),
            private_key_path: Some(PathBuf::from(r"C:\secret\id_ed25519")),
            password_credential_ref: Some("password-secret-ref".to_owned()),
            key_passphrase_credential_ref: Some("passphrase-secret-ref".to_owned()),
        };
        let debug = format!("{binding:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("id_ed25519"));
        assert!(!debug.contains("password-secret-ref"));
        assert!(!debug.contains("passphrase-secret-ref"));
    }
}
