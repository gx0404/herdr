//! 有界、临时的阅读快照。PTY 与实时 pane 渲染不等待客户端结束选字。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::api::schema::{Method, ResponseResult};
use crate::terminal::text_snapshot::FrozenText;

const TTL: Duration = Duration::from_secs(300);
const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 32;
type Result = std::result::Result<ResponseResult, (&'static str, String)>;

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
            .ok_or((
                "snapshot_expired",
                "阅读快照已释放或过期，请重新选择".into(),
            ))?;
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
                    return Err((
                        "snapshot_capacity",
                        "阅读快照已达到容量上限，请结束其他阅读后重试".into(),
                    ));
                }
                if let Some(owner) = owner {
                    store.release_owner(owner);
                }
                store.serial = store
                    .serial
                    .checked_add(1)
                    .ok_or(("snapshot_capacity", "阅读快照编号已耗尽".into()))?;
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
                        .ok_or(("snapshot_range", "无法读取快照视口".into()))?,
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
                    .ok_or(("snapshot_range", "请求行超出阅读快照范围".into()))?;
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
                    .ok_or(("snapshot_range", "选区超出阅读快照范围".into()))?;
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
            _ => Err(("invalid_request", "不是阅读快照请求".into())),
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
}
