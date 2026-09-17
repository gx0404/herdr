//! Cross-endpoint input broadcast: the explicit target set.
//!
//! Broadcasting pane input to several machines at once is dangerous by
//! nature, so the mechanism is gated twice: the set is empty and disabled by
//! default, and sending requires an enabled, non-empty set. Each endpoint
//! (Local or one saved machine) contributes at most one pane. The set is
//! client-local state, persisted atomically next to the endpoint catalog.
//!
//! This module owns registration, query, and clearing only. Sending reuses
//! the existing per-endpoint JSON API channels (`pane.send-input` /
//! `pane.send-text`); the CLI executor lives in `cli::broadcast` and the
//! shell UI lane is a later stage.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::ProfileId;

const BROADCAST_VERSION: u32 = 1;
const MAX_BROADCAST_TARGETS: usize = 32;
const MAX_PANE_ID_BYTES: usize = 128;

/// One broadcast destination: a pane on Local (`machine: None`) or on a
/// saved SSH machine (`machine: Some(profile id)`). The profile id is
/// resolved and validated at registration time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BroadcastTarget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) machine: Option<ProfileId>,
    pub(crate) pane_id: String,
}

/// The persisted broadcast target set. `enabled` defaults to off and is the
/// explicit safety gate every sender must check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BroadcastSet {
    version: u32,
    #[serde(default)]
    pub(crate) enabled: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    targets: Vec<BroadcastTarget>,
}

impl Default for BroadcastSet {
    fn default() -> Self {
        Self {
            version: BROADCAST_VERSION,
            enabled: false,
            targets: Vec::new(),
        }
    }
}

impl BroadcastSet {
    pub(crate) fn load() -> Result<Self, String> {
        Self::load_from_path(&broadcast_path())
    }

    pub(crate) fn store(&self) -> Result<(), String> {
        self.store_to_path(&broadcast_path())
    }

    fn load_from_path(path: &std::path::Path) -> Result<Self, String> {
        let content = match std::fs::read(path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(format!(
                    "failed to read broadcast targets {}: {error}",
                    path.display()
                ));
            }
        };
        let set: Self = serde_json::from_slice(&content)
            .map_err(|error| format!("stored broadcast targets are invalid: {error}"))?;
        if set.version != BROADCAST_VERSION {
            return Err(format!(
                "unsupported broadcast target version {}; expected {BROADCAST_VERSION}",
                set.version
            ));
        }
        set.validate()?;
        Ok(set)
    }

    fn store_to_path(&self, path: &std::path::Path) -> Result<(), String> {
        self.validate()?;
        let content = serde_json::to_vec_pretty(self)
            .map_err(|error| format!("failed to encode broadcast targets: {error}"))?;
        super::catalog::store_private_json(path, &content, "broadcast targets")
    }

    pub(crate) fn targets(&self) -> &[BroadcastTarget] {
        &self.targets
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    /// Registers one pane for an endpoint. Each endpoint appears at most
    /// once: re-registering is an error so changing a machine's pane is
    /// always an explicit remove-then-add.
    pub(crate) fn add_target(&mut self, target: BroadcastTarget) -> Result<(), String> {
        if self
            .targets
            .iter()
            .any(|existing| existing.machine == target.machine)
        {
            return Err(match &target.machine {
                Some(id) => format!(
                    "machine {id} already has a broadcast pane; remove it before adding another"
                ),
                None => {
                    "Local already has a broadcast pane; remove it before adding another".into()
                }
            });
        }
        self.targets.push(target);
        self.validate().inspect_err(|_| {
            self.targets.pop();
        })
    }

    /// Removes the target with the 1-based number shown by status output.
    pub(crate) fn remove_target(&mut self, number: usize) -> Result<BroadcastTarget, String> {
        if number == 0 || number > self.targets.len() {
            return Err(format!(
                "broadcast target number must be between 1 and {}",
                self.targets.len()
            ));
        }
        Ok(self.targets.remove(number - 1))
    }

    pub(crate) fn clear(&mut self) {
        self.targets.clear();
    }

    fn validate(&self) -> Result<(), String> {
        if self.targets.len() > MAX_BROADCAST_TARGETS {
            return Err(format!(
                "at most {MAX_BROADCAST_TARGETS} broadcast targets are allowed"
            ));
        }
        for target in &self.targets {
            if target.pane_id.is_empty()
                || target.pane_id.len() > MAX_PANE_ID_BYTES
                || target.pane_id.chars().any(char::is_control)
            {
                return Err(format!(
                    "broadcast pane id must be 1..{MAX_PANE_ID_BYTES} bytes and contain no control characters"
                ));
            }
        }
        Ok(())
    }
}

pub(crate) fn broadcast_path() -> PathBuf {
    crate::config::state_dir()
        .join("client")
        .join("broadcast.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine_target(pane_id: &str) -> BroadcastTarget {
        BroadcastTarget {
            machine: Some(ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap()),
            pane_id: pane_id.into(),
        }
    }

    fn local_target(pane_id: &str) -> BroadcastTarget {
        BroadcastTarget {
            machine: None,
            pane_id: pane_id.into(),
        }
    }

    #[test]
    fn default_set_is_disabled_and_empty() {
        let set = BroadcastSet::default();
        assert!(!set.enabled);
        assert!(set.is_empty());
    }

    #[test]
    fn add_target_allows_one_pane_per_endpoint() {
        let mut set = BroadcastSet::default();
        set.add_target(local_target("w1:p1")).unwrap();
        set.add_target(machine_target("w2:p3")).unwrap();
        assert_eq!(set.targets().len(), 2);
        for duplicate in [local_target("w9:p9"), machine_target("w4:p1")] {
            let error = set.add_target(duplicate).unwrap_err();
            assert!(error.contains("already has a broadcast pane"), "{error}");
            assert_eq!(set.targets().len(), 2);
        }
    }

    #[test]
    fn validation_rejects_bad_pane_ids_and_oversized_sets() {
        let mut set = BroadcastSet::default();
        for bad in ["", "w1:p1\n"] {
            assert!(set
                .add_target(BroadcastTarget {
                    machine: None,
                    pane_id: bad.into(),
                })
                .is_err());
        }
        for index in 0..MAX_BROADCAST_TARGETS {
            set.add_target(BroadcastTarget {
                machine: Some(ProfileId::parse(format!("{index:032x}")).unwrap()),
                pane_id: "w1:p1".into(),
            })
            .unwrap();
        }
        assert!(set
            .add_target(BroadcastTarget {
                machine: Some(ProfileId::parse("ffffffffffffffffffffffffffffffff").unwrap()),
                pane_id: "w1:p1".into(),
            })
            .is_err());
    }

    #[test]
    fn remove_target_uses_the_status_numbering() {
        let mut set = BroadcastSet::default();
        set.add_target(local_target("w1:p1")).unwrap();
        set.add_target(machine_target("w2:p2")).unwrap();
        assert!(set.remove_target(0).is_err());
        assert!(set.remove_target(3).is_err());
        let removed = set.remove_target(1).unwrap();
        assert_eq!(removed, local_target("w1:p1"));
        set.clear();
        assert!(set.is_empty());
    }

    #[test]
    fn store_round_trips_atomically_and_rejects_newer_versions() {
        let dir = std::env::temp_dir().join(format!(
            "herdr-broadcast-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("broadcast.json");
        let mut set = BroadcastSet {
            enabled: true,
            ..BroadcastSet::default()
        };
        set.add_target(local_target("w1:p1")).unwrap();
        set.add_target(machine_target("w2:p2")).unwrap();
        set.store_to_path(&path).unwrap();
        let loaded = BroadcastSet::load_from_path(&path).unwrap();
        assert_eq!(loaded, set);

        std::fs::write(&path, br#"{"version":2,"enabled":false}"#).unwrap();
        assert!(BroadcastSet::load_from_path(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
