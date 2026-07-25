use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Stable identity shown to the user and persisted after explicit trust.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostKeyIdentity {
    pub algorithm: String,
    pub sha256_fingerprint: String,
}

impl HostKeyIdentity {
    #[must_use]
    pub fn new(algorithm: impl Into<String>, sha256_fingerprint: impl Into<String>) -> Self {
        Self {
            algorithm: algorithm.into(),
            sha256_fingerprint: sha256_fingerprint.into(),
        }
    }

    pub fn validate(&self) -> Result<(), HostKeyIdentityError> {
        let algorithm = self.algorithm.as_bytes();
        if algorithm.is_empty()
            || algorithm.len() > 128
            || !algorithm
                .iter()
                .all(|byte| byte.is_ascii_graphic() && !byte.is_ascii_whitespace())
        {
            return Err(HostKeyIdentityError::Algorithm);
        }

        let Some(encoded) = self.sha256_fingerprint.strip_prefix("SHA256:") else {
            return Err(HostKeyIdentityError::Fingerprint);
        };
        if encoded.len() < 16
            || encoded.len() > 128
            || !encoded
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
        {
            return Err(HostKeyIdentityError::Fingerprint);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum HostKeyIdentityError {
    #[error("invalid SSH host-key algorithm")]
    Algorithm,
    #[error("invalid SSH SHA-256 host-key fingerprint")]
    Fingerprint,
}

/// Result of comparing a presented key with the locally trusted identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostKeyDecision {
    Trusted(HostKeyIdentity),
    Unknown {
        presented: HostKeyIdentity,
    },
    Changed {
        expected: HostKeyIdentity,
        presented: HostKeyIdentity,
    },
}

/// Applies Lumen's strict host-key policy.
///
/// An absent trust record is never accepted automatically. A changed
/// algorithm is treated the same as a changed fingerprint and is hard-rejected.
#[must_use]
pub fn decide_host_key(
    expected: Option<&HostKeyIdentity>,
    presented: &HostKeyIdentity,
) -> HostKeyDecision {
    match expected {
        Some(expected) if expected == presented => HostKeyDecision::Trusted(presented.clone()),
        Some(expected) => HostKeyDecision::Changed {
            expected: expected.clone(),
            presented: presented.clone(),
        },
        None => HostKeyDecision::Unknown {
            presented: presented.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(algorithm: &str, fingerprint: &str) -> HostKeyIdentity {
        HostKeyIdentity::new(algorithm, fingerprint)
    }

    #[test]
    fn only_exact_trusted_key_is_accepted() {
        let trusted = key("ssh-ed25519", "SHA256:abcdefghijklmnopqrstuv");
        assert_eq!(
            decide_host_key(Some(&trusted), &trusted),
            HostKeyDecision::Trusted(trusted)
        );
    }

    #[test]
    fn absent_key_is_unknown_not_trusted() {
        let presented = key("ssh-ed25519", "SHA256:abcdefghijklmnopqrstuv");
        assert_eq!(
            decide_host_key(None, &presented),
            HostKeyDecision::Unknown { presented }
        );
    }

    #[test]
    fn fingerprint_or_algorithm_change_is_hard_change() {
        let expected = key("ssh-ed25519", "SHA256:abcdefghijklmnopqrstuv");
        for presented in [
            key("ssh-ed25519", "SHA256:zyxwvutsrqponmlkjihgfe"),
            key("rsa-sha2-512", "SHA256:abcdefghijklmnopqrstuv"),
        ] {
            assert_eq!(
                decide_host_key(Some(&expected), &presented),
                HostKeyDecision::Changed {
                    expected: expected.clone(),
                    presented,
                }
            );
        }
    }

    #[test]
    fn validates_openssh_sha256_shape() {
        assert!(key("ssh-ed25519", "SHA256:abcdefghijklmnopqrstuv")
            .validate()
            .is_ok());
        assert_eq!(
            key("ssh-ed25519", "MD5:00:11").validate(),
            Err(HostKeyIdentityError::Fingerprint)
        );
    }
}
