use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::model::ProfileId;

/// SSH 库存的本地/账号作用域。账号字符串只会编码，不会直接成为路径片段。
#[derive(Clone, PartialEq, Eq)]
pub enum StorageScope {
    Local,
    Account(String),
}

impl StorageScope {
    pub fn directory(&self, data_root: &Path) -> Result<PathBuf, &'static str> {
        let ssh_root = data_root.join("ssh");
        match self {
            Self::Local => Ok(ssh_root.join("unclaimed")),
            Self::Account(account_id) => {
                let account_id = canonical_account_id(account_id)?;
                Ok(ssh_root
                    .join("accounts")
                    .join(format!("acct_{}", account_id.replace('-', ""))))
            }
        }
    }

    pub fn canonical_account_id(&self) -> Result<Option<String>, &'static str> {
        match self {
            Self::Local => Ok(None),
            Self::Account(account_id) => canonical_account_id(account_id).map(Some),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 账号id必须是canonical_uuid且目录组件固定长度() {
        let root = Path::new(r"C:\LumenData");
        let account_a = StorageScope::Account("550E8400-E29B-41D4-A716-446655440000".to_owned())
            .directory(root)
            .unwrap();
        let account_b = StorageScope::Account("550e8400-e29b-41d4-a716-446655440001".to_owned())
            .directory(root)
            .unwrap();

        assert!(account_a.starts_with(root.join("ssh").join("accounts")));
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
            assert!(StorageScope::Account(invalid.to_owned())
                .directory(root)
                .is_err());
        }
    }

    #[test]
    fn 本地作用域与账号作用域目录隔离() {
        let root = Path::new(r"D:\data");
        let local = StorageScope::Local.directory(root).unwrap();
        let account = StorageScope::Account("550e8400-e29b-41d4-a716-446655440000".to_owned())
            .directory(root)
            .unwrap();
        assert_ne!(local, account);
        assert_eq!(local, root.join("ssh").join("unclaimed"));
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
