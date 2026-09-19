//! 只持久化账号引用、公开身份与 pane 绑定，禁止写入凭据或用量报文。
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 公开身份表上限：账号清单本身 ≤128，这里只是防止旧文件里的残留无限累积。
pub(super) const MAX_IDENTITIES: usize = 256;
/// 单条公开身份的最大长度，与探测侧的 `filter(|v| v.len() <= 256)` 一致。
const MAX_IDENTITY_LEN: usize = 256;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Saved {
    pub bindings: HashMap<String, String>,
    pub identities: HashMap<String, String>,
}

impl Saved {
    /// 只保留仍在账号清单里、且形状合法的公开身份；超出上限时按账号 id 字典序截断，
    /// 让结果与 HashMap 的遍历顺序无关。
    pub(super) fn prune_identities(&mut self, keep: impl Fn(&str) -> bool) {
        self.identities.retain(|id, identity| {
            keep(id)
                && !identity.is_empty()
                && identity.len() <= MAX_IDENTITY_LEN
                && !identity.chars().any(char::is_control)
        });
        if self.identities.len() > MAX_IDENTITIES {
            let mut ids = self.identities.keys().cloned().collect::<Vec<_>>();
            ids.sort();
            for id in ids.into_iter().skip(MAX_IDENTITIES) {
                self.identities.remove(&id);
            }
        }
    }
}

/// 本 server 实例的持久化文件：按 socket 路径哈希隔离。
pub(super) fn path() -> PathBuf {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(crate::api::socket_path().to_string_lossy().as_bytes());
    crate::config::state_dir()
        .join("account-usage")
        .join(format!("{hash:x}.json"))
}

pub(super) fn load(path: &Path) -> Saved {
    std::fs::read(path)
        .ok()
        .filter(|bytes| bytes.len() <= 1024 * 1024)
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// 脏检查：与上次成功落盘的内容一致时跳过写文件。返回本次是否真的写成功。
/// 写失败时不推进 `last_stored`，下一次调用会带着相同内容重试，而不是把失败当成已落盘。
pub(super) fn store_if_changed(
    path: &Path,
    saved: &Saved,
    last_stored: &mut Option<Saved>,
) -> bool {
    if !changed(saved, last_stored.as_ref()) {
        return false;
    }
    match store(path, saved) {
        Ok(()) => {
            *last_stored = Some(saved.clone());
            true
        }
        Err(error) => {
            tracing::warn!(
                %error,
                path = %path.display(),
                "保存账号绑定失败，下次变更或落盘时重试"
            );
            false
        }
    }
}

fn changed(saved: &Saved, last_stored: Option<&Saved>) -> bool {
    last_stored != Some(saved)
}

fn store(path: &Path, saved: &Saved) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec(saved).map_err(std::io::Error::other)?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temporary, bytes)?;
    crate::platform::replace_file(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_is_skipped_until_the_saved_state_changes() {
        let mut saved = Saved::default();
        saved
            .bindings
            .insert("pane-1".into(), "claude:default".into());
        // 首次没有基准：视为已变化。
        assert!(changed(&saved, None));
        let last = Some(saved.clone());
        assert!(!changed(&saved, last.as_ref()));
        saved
            .identities
            .insert("claude:default".into(), "me@example.test".into());
        assert!(changed(&saved, last.as_ref()));
    }

    #[test]
    fn failed_stores_are_retried_on_the_next_call_instead_of_being_marked_persisted() {
        let base = std::env::temp_dir().join(format!(
            "herdr-usage-persist-{}-{}",
            std::process::id(),
            crate::server::observability::now_ms()
        ));
        std::fs::create_dir_all(&base).unwrap();
        // 父目录位置被一个普通文件占住：create_dir_all 失败，模拟权限/磁盘类瞬时错误。
        let blocker = base.join("blocked");
        std::fs::write(&blocker, b"x").unwrap();
        let unwritable = blocker.join("state.json");
        let mut saved = Saved::default();
        saved
            .bindings
            .insert("pane-1".into(), "claude:default".into());
        let mut last = None;
        assert!(!store_if_changed(&unwritable, &saved, &mut last));
        assert_eq!(last, None, "写失败不得记为已落盘");
        assert!(!unwritable.exists());

        // 同样的内容换到可写路径：因为 last 未推进，脏检查仍认为需要写。
        let writable = base.join("ok").join("state.json");
        assert!(store_if_changed(&writable, &saved, &mut last));
        assert_eq!(last.as_ref(), Some(&saved));
        assert_eq!(load(&writable), saved);
        assert!(
            !store_if_changed(&writable, &saved, &mut last),
            "内容未变不再写"
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn identities_are_pruned_to_configured_accounts_and_capped() {
        let mut saved = Saved::default();
        saved
            .identities
            .insert("gone:default".into(), "old@example.test".into());
        saved
            .identities
            .insert("claude:default".into(), "me@example.test".into());
        saved
            .identities
            .insert("codex:default".into(), "x".repeat(MAX_IDENTITY_LEN + 1));
        saved
            .identities
            .insert("kimi:default".into(), "bad\u{7}id".into());
        saved.identities.insert("omp:default".into(), String::new());
        let configured = [
            "claude:default",
            "codex:default",
            "kimi:default",
            "omp:default",
        ];
        saved.prune_identities(|id| configured.contains(&id));
        assert_eq!(
            saved.identities.keys().collect::<Vec<_>>(),
            vec!["claude:default"]
        );

        let mut saved = Saved::default();
        for index in 0..(MAX_IDENTITIES + 5) {
            saved
                .identities
                .insert(format!("acct-{index:04}"), "id".into());
        }
        saved.prune_identities(|_| true);
        assert_eq!(saved.identities.len(), MAX_IDENTITIES);
        assert!(saved.identities.contains_key("acct-0000"));
        assert!(!saved.identities.contains_key("acct-0260"));
    }
}
