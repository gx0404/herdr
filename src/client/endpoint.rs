use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

mod activation;
mod broadcast;
mod catalog;
mod control;
mod health;
mod message_policy;
mod registry;
mod session_log;
mod snippets;
mod supervisor;
mod writer;

pub(crate) use activation::*;
pub(crate) use broadcast::*;
pub(crate) use catalog::*;
pub(crate) use control::*;
pub(crate) use message_policy::*;
pub(crate) use registry::*;
pub(crate) use session_log::*;
pub(crate) use snippets::*;
pub(crate) use supervisor::*;
pub(crate) use writer::NativeEndpointTransport;

const PROFILE_ID_BYTES: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct ProfileId(String);

impl ProfileId {
    pub(crate) fn parse(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if value.len() != PROFILE_ID_BYTES * 2
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err("endpoint profile id must be 32 lowercase hexadecimal characters".into());
        }
        Ok(Self(value))
    }

    pub(crate) fn generate() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);

        let sequence = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let digest = Sha256::digest(format!("{}:{now}:{sequence}", std::process::id()).as_bytes());
        Self(
            digest[..PROFILE_ID_BYTES]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        )
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProfileId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum ClientEndpointId {
    Local,
    Ssh(ProfileId),
}

impl ClientEndpointId {
    pub(crate) fn is_local(&self) -> bool {
        matches!(self, Self::Local)
    }

    pub(crate) fn storage_key(&self) -> String {
        match self {
            Self::Local => "local".into(),
            Self::Ssh(profile_id) => format!("ssh:{profile_id}"),
        }
    }

    /// `storage_key` 的反向解析（偏好文件里的键）。未知或损坏的值返回 `None`：
    /// 与其它偏好键同一口径——坏值按未设置处理，不让整份偏好失效（STATE-05）。
    pub(crate) fn from_storage_key(key: &str) -> Option<Self> {
        if key == "local" {
            return Some(Self::Local);
        }
        key.strip_prefix("ssh:")
            .and_then(|profile_id| ProfileId::parse(profile_id).ok())
            .map(Self::Ssh)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClientEndpointStatus {
    Connecting,
    Online,
    Reconnecting,
    Attention,
    Disabled,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_storage_keys_round_trip_and_reject_junk() {
        let remote = ClientEndpointId::Ssh(
            ProfileId::parse("0123456789abcdef0123456789abcdef").expect("profile id"),
        );
        for endpoint_id in [ClientEndpointId::Local, remote] {
            let key = endpoint_id.storage_key();
            assert_eq!(ClientEndpointId::from_storage_key(&key), Some(endpoint_id));
        }
        for junk in ["", "local:", "ssh:", "ssh:not-a-profile-id", "remote"] {
            assert_eq!(ClientEndpointId::from_storage_key(junk), None, "{junk}");
        }
    }

    #[test]
    fn profile_ids_are_opaque_and_stable_when_parsed() {
        let first = ProfileId::generate();
        let second = ProfileId::generate();
        assert_ne!(first, second);
        assert_eq!(ProfileId::parse(first.to_string()).unwrap(), first);
        assert_eq!(first.as_str().len(), 32);
    }

    #[test]
    fn endpoint_storage_keys_do_not_contain_ssh_targets() {
        let profile = ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap();
        assert_eq!(
            ClientEndpointId::Ssh(profile).storage_key(),
            "ssh:0123456789abcdef0123456789abcdef"
        );
    }
}
