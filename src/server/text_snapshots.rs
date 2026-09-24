//! 有界、临时的阅读快照。PTY 与实时 pane 渲染不等待客户端结束选字。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::api::schema::{Method, ResponseResult};
use crate::terminal::text_snapshot::FrozenText;

const TTL: Duration = Duration::from_secs(300);
const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 32;
type Result = std::result::Result<ResponseResult, (&'static str, String)>;

/// 阅读快照的错误说明按 server 的界面语言给出（文档终审 D7）；错误码不变。
fn texts() -> &'static crate::i18n::RuntimeMessageTexts {
    &crate::i18n::texts().runtime
}

struct Entry {
    owner: Option<u64>,
    pane_id: String,
    touched: Instant,
    retained: bool,
    text: FrozenText,
}

#[derive(Default)]
pub(super) struct Store {
    entries: HashMap<String, Entry>,
    serial: u64,
    next_expiry: Option<Instant>,
}

pub(super) fn handles(method: &Method) -> bool {
    matches!(
        method,
        Method::PaneTextSnapshotCapture(_)
            | Method::PaneTextSnapshotRead(_)
            | Method::PaneTextSnapshotSelection(_)
            | Method::PaneTextSnapshotRetain(_)
            | Method::PaneTextSnapshotRelease(_)
    )
}

impl Store {
    pub(super) fn expire(&mut self, now: Instant) {
        if self.next_expiry.is_some_and(|next| now < next) {
            return;
        }
        self.entries.retain(|_, entry| {
            (entry.owner.is_some() && entry.retained)
                || now.duration_since(entry.touched)
                    < if entry.owner.is_some() {
                        Duration::from_secs(60)
                    } else {
                        TTL
                    }
        });
        self.next_expiry = Some(now + Duration::from_secs(30));
    }

    pub(super) fn release_owner(&mut self, owner: u64) {
        self.entries.retain(|_, entry| entry.owner != Some(owner));
    }

    fn get(
        &mut self,
        id: &str,
        owner: Option<u64>,
    ) -> std::result::Result<&mut Entry, (&'static str, String)> {
        let entry = self
            .entries
            .get_mut(id)
            .filter(|entry| entry.owner == owner)
            .ok_or_else(|| ("snapshot_expired", texts().snapshot_expired.into()))?;
        entry.touched = Instant::now();
        Ok(entry)
    }
}

impl super::headless::HeadlessServer {
    pub(super) fn text_snapshot_request(&mut self, method: &Method, owner: Option<u64>) -> Result {
        let now = Instant::now();
        self.text_snapshots.entries.retain(|_, entry| {
            (entry.owner.is_some() && entry.retained)
                || now.duration_since(entry.touched)
                    < if entry.owner.is_some() {
                        Duration::from_secs(60)
                    } else {
                        TTL
                    }
        });
        match method {
            Method::PaneTextSnapshotCapture(params) => {
                let text = self.app.capture_pane_text_snapshot(&params.pane_id)?;
                let store = &mut self.text_snapshots;
                let occupied: usize = store
                    .entries
                    .values()
                    .filter(|entry| owner.is_none() || entry.owner != owner)
                    .map(|entry| entry.text.bytes())
                    .sum();
                if text.bytes().saturating_add(occupied) > MAX_BYTES
                    || store
                        .entries
                        .values()
                        .filter(|entry| owner.is_none() || entry.owner != owner)
                        .count()
                        >= MAX_ENTRIES
                {
                    return Err(("snapshot_capacity", texts().snapshot_capacity.into()));
                }
                if let Some(owner) = owner {
                    store.release_owner(owner);
                }
                store.serial = store
                    .serial
                    .checked_add(1)
                    .ok_or_else(|| ("snapshot_capacity", texts().snapshot_ids_exhausted.into()))?;
                let id = format!("{}:text:{}", self.client_shell_boot_id, store.serial);
                let result = ResponseResult::PaneTextSnapshot {
                    snapshot_id: id.clone(),
                    pane_id: params.pane_id.clone(),
                    boot_id: self.client_shell_boot_id.clone(),
                    text: Box::new(
                        text.window(
                            text.viewport_start.saturating_sub(16).max(text.range_start),
                            text.viewport_rows.saturating_add(32),
                        )
                        .ok_or_else(|| {
                            (
                                "snapshot_range",
                                texts().snapshot_viewport_unreadable.into(),
                            )
                        })?,
                    ),
                };
                store.entries.insert(
                    id,
                    Entry {
                        owner,
                        pane_id: params.pane_id.clone(),
                        touched: now,
                        retained: false,
                        text,
                    },
                );
                Ok(result)
            }
            Method::PaneTextSnapshotRead(params) => {
                let entry = self.text_snapshots.get(&params.snapshot_id, owner)?;
                let text = entry
                    .text
                    .window(params.start_row, params.rows.clamp(1, 2048))
                    .filter(|text| !text.rows.is_empty())
                    .ok_or_else(|| ("snapshot_range", texts().snapshot_rows_out_of_range.into()))?;
                Ok(ResponseResult::PaneTextSnapshot {
                    snapshot_id: params.snapshot_id.clone(),
                    pane_id: entry.pane_id.clone(),
                    boot_id: self.client_shell_boot_id.clone(),
                    text: Box::new(text),
                })
            }
            Method::PaneTextSnapshotSelection(params) => {
                let entry = self.text_snapshots.get(&params.snapshot_id, owner)?;
                let text = entry
                    .text
                    .selection(
                        (params.anchor.row, params.anchor.col),
                        (params.cursor.row, params.cursor.col),
                    )
                    .ok_or_else(|| {
                        (
                            "snapshot_range",
                            texts().snapshot_selection_out_of_range.into(),
                        )
                    })?;
                Ok(ResponseResult::PaneTextSnapshotSelection {
                    snapshot_id: params.snapshot_id.clone(),
                    text,
                })
            }
            Method::PaneTextSnapshotRetain(params) => {
                self.text_snapshots
                    .get(&params.snapshot_id, owner)?
                    .retained = true;
                Ok(ResponseResult::PaneTextSnapshotRetained {
                    snapshot_id: params.snapshot_id.clone(),
                })
            }
            Method::PaneTextSnapshotRelease(params) => {
                if self
                    .text_snapshots
                    .entries
                    .get(&params.snapshot_id)
                    .is_some_and(|entry| entry.owner == owner)
                {
                    self.text_snapshots.entries.remove(&params.snapshot_id);
                }
                Ok(ResponseResult::PaneTextSnapshotReleased {
                    snapshot_id: params.snapshot_id.clone(),
                })
            }
            _ => Err(("invalid_request", texts().snapshot_invalid_request.into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(now: Instant, retained: bool) -> Entry {
        Entry {
            owner: Some(7),
            pane_id: "pane_1".into(),
            touched: now,
            retained,
            text: FrozenText {
                cols: 1,
                viewport_rows: 1,
                viewport_start: 0,
                row_origin: 0,
                range_start: 0,
                range_end: 1,
                total_rows: 1,
                alternate_screen: false,
                content_revision: 0,
                truncated: false,
                rows: vec![],
            },
        }
    }

    #[test]
    fn unacknowledged_capture_expires_even_when_connection_stays_open() {
        let now = Instant::now();
        let mut store = Store::default();
        store.entries.insert("late".into(), entry(now, false));
        store.entries.insert("reading".into(), entry(now, true));
        store.expire(now + Duration::from_secs(61));
        assert!(!store.entries.contains_key("late"));
        assert!(store.entries.contains_key("reading"));
        assert!(store.get("reading", Some(8)).is_err());
        store.release_owner(7);
        assert!(store.entries.is_empty());
    }

    /// 文档终审 D7：阅读快照的错误说明按界面语言给出——英文界面不含 CJK，中文界面是
    /// 中文；错误码不随语言变化。
    #[test]
    fn snapshot_errors_follow_the_interface_language() {
        use crate::i18n::{has_cjk, lang_guard, Lang};
        let mut store = Store::default();
        for (lang, chinese) in [(Lang::En, false), (Lang::ZhCn, true)] {
            let _guard = lang_guard(lang);
            let Err((code, message)) = store.get("missing", None) else {
                panic!("缺失的阅读快照应报错");
            };
            assert_eq!(code, "snapshot_expired");
            assert_eq!(message, texts().snapshot_expired);
            assert_eq!(has_cjk(&message), chinese, "{lang:?}: {message}");
        }
    }
}
