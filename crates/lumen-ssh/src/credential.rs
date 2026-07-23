use std::path::PathBuf;

use zeroize::Zeroize;

/// A secret held in memory and cleared before its allocation is released.
///
/// This type intentionally does not implement `Debug`, `Display`, `Clone`,
/// `Serialize`, or `Deserialize`.
pub struct SecretString {
    value: String,
}

impl SecretString {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    pub(crate) fn expose(&self) -> &str {
        &self.value
    }
}

impl From<String> for SecretString {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for SecretString {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl Zeroize for SecretString {
    fn zeroize(&mut self) {
        self.value.zeroize();
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        self.zeroize();
    }
}

/// Local private-key material used for one authentication attempt.
///
/// The key path and passphrase stay client-local. This type intentionally has
/// no `Debug`/serialization implementation.
pub struct PrivateKeyCredential {
    pub path: PathBuf,
    pub passphrase: Option<SecretString>,
}

impl PrivateKeyCredential {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, passphrase: Option<SecretString>) -> Self {
        Self {
            path: path.into(),
            passphrase,
        }
    }
}

/// Authentication material for a connection.
///
/// Secrets are moved to the connection thread and dropped immediately after
/// authentication. The enum intentionally has no `Debug`/serialization
/// implementation.
pub enum Credential {
    Password(SecretString),
    PrivateKey(PrivateKeyCredential),
    Agent,
}

impl Credential {
    #[must_use]
    pub fn password(password: impl Into<SecretString>) -> Self {
        Self::Password(password.into())
    }

    #[must_use]
    pub fn private_key(path: impl Into<PathBuf>, passphrase: Option<SecretString>) -> Self {
        Self::PrivateKey(PrivateKeyCredential::new(path, passphrase))
    }

    #[must_use]
    pub const fn agent() -> Self {
        Self::Agent
    }

    pub(crate) const fn kind_name(&self) -> &'static str {
        match self {
            Self::Password(_) => "password",
            Self::PrivateKey(_) => "private-key",
            Self::Agent => "agent",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_zeroize_clears_secret() {
        let mut secret = SecretString::new("correct horse battery staple");
        secret.zeroize();
        assert!(secret.is_empty());
    }
}
