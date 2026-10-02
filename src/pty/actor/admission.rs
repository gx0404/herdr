use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};

use bytes::Bytes;
use tracing::warn;

/// Maximum number of queued logical input items, including text-plus-Enter submissions.
pub(super) const INPUT_ITEM_LIMIT: usize = 1024;
/// Total input payload bytes held by admission permits, including framing and queued keystrokes.
pub(super) const INPUT_BYTE_LIMIT: usize = 16 * 1024 * 1024;
/// Maximum number of terminal response payloads retained across control and parser output.
pub(super) const RESPONSE_ITEM_LIMIT: usize = 256;
/// Total terminal response bytes held by admission permits.
pub(super) const RESPONSE_BYTE_LIMIT: usize = 1024 * 1024;

// Each successful reservation owns one item and its exact byte count until the
// permit and every `Bytes` wrapper carrying that reservation have been dropped.

#[derive(Debug)]
pub(super) struct Admission {
    accepting: AtomicBool,
    item_limit: usize,
    byte_limit: usize,
    items: AtomicUsize,
    bytes: AtomicUsize,
    warned: AtomicBool,
}

impl Admission {
    pub(super) fn new(item_limit: usize, byte_limit: usize) -> Arc<Self> {
        Arc::new(Self {
            accepting: AtomicBool::new(true),
            item_limit,
            byte_limit,
            items: AtomicUsize::new(0),
            bytes: AtomicUsize::new(0),
            warned: AtomicBool::new(false),
        })
    }

    pub(super) fn input() -> Arc<Self> {
        Self::new(INPUT_ITEM_LIMIT, INPUT_BYTE_LIMIT)
    }

    pub(super) fn responses() -> Arc<Self> {
        Self::new(RESPONSE_ITEM_LIMIT, RESPONSE_BYTE_LIMIT)
    }

    pub(super) fn try_reserve(self: &Arc<Self>, bytes: usize) -> Option<Permit> {
        if !self.accepting.load(Ordering::Acquire) || bytes > self.byte_limit {
            return None;
        }
        self.items
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current
                    .checked_add(1)
                    .filter(|next| *next <= self.item_limit)
            })
            .ok()?;
        if !self.accepting.load(Ordering::Acquire) {
            self.items.fetch_sub(1, Ordering::Release);
            return None;
        }
        if self
            .bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current
                    .checked_add(bytes)
                    .filter(|next| *next <= self.byte_limit)
            })
            .is_err()
        {
            self.items.fetch_sub(1, Ordering::Release);
            return None;
        }
        if !self.accepting.load(Ordering::Acquire) {
            self.bytes.fetch_sub(bytes, Ordering::Release);
            self.items.fetch_sub(1, Ordering::Release);
            return None;
        }
        Some(Permit {
            _lease: Arc::new(Lease {
                admission: Arc::clone(self),
                bytes,
            }),
        })
    }

    pub(super) fn close(&self) {
        self.accepting.store(false, Ordering::Release);
    }

    pub(super) fn is_accepting(&self) -> bool {
        self.accepting.load(Ordering::Acquire)
    }

    pub(super) fn admit_response(self: &Arc<Self>, bytes: Bytes) -> Option<Bytes> {
        if bytes.is_empty() || !self.is_accepting() {
            return None;
        }
        let Some(permit) = self.try_reserve(bytes.len()) else {
            if !self.warned.swap(true, Ordering::AcqRel) {
                warn!(
                    item_limit = self.item_limit,
                    byte_limit = self.byte_limit,
                    "PTY response backlog full; dropping whole terminal response"
                );
            }
            return None;
        };
        Some(permit.wrap(bytes))
    }

    #[cfg(test)]
    pub(super) fn in_use(&self) -> (usize, usize) {
        (
            self.items.load(Ordering::Acquire),
            self.bytes.load(Ordering::Acquire),
        )
    }
}

#[derive(Debug, Clone)]
pub(super) struct Permit {
    _lease: Arc<Lease>,
}

impl Permit {
    pub(super) fn wrap(&self, bytes: Bytes) -> Bytes {
        Bytes::from_owner(AdmittedBytes {
            bytes,
            _permit: self.clone(),
        })
    }
}

#[derive(Debug)]
struct Lease {
    admission: Arc<Admission>,
    bytes: usize,
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.admission
            .bytes
            .fetch_sub(self.bytes, Ordering::Release);
        if self.admission.items.fetch_sub(1, Ordering::Release) == 1 {
            self.admission.warned.store(false, Ordering::Release);
        }
    }
}

struct AdmittedBytes {
    bytes: Bytes,
    _permit: Permit,
}

impl AsRef<[u8]> for AdmittedBytes {
    fn as_ref(&self) -> &[u8] {
        self.bytes.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloned_payload_keeps_item_and_bytes_until_last_owner_drops() {
        let admission = Admission::new(1, 4);
        let permit = admission.try_reserve(4).expect("first permit");
        let payload = permit.wrap(Bytes::from_static(b"text"));
        let forwarded = payload.clone();
        drop((permit, payload));
        assert!(admission.try_reserve(1).is_none());
        assert_eq!(admission.in_use(), (1, 4));
        drop(forwarded);
        assert_eq!(admission.in_use(), (0, 0));
        assert!(admission.try_reserve(4).is_some());
    }

    #[test]
    fn empty_enter_keeps_submission_permit_during_delay() {
        let admission = Admission::new(1, 4);
        let permit = admission.try_reserve(4).unwrap();
        let text = permit.wrap(Bytes::from_static(b"text"));
        let enter = permit.wrap(Bytes::new());
        drop((permit, text));
        assert_eq!(admission.in_use(), (1, 4));
        drop(enter);
        assert_eq!(admission.in_use(), (0, 0));
    }

    #[test]
    fn byte_limit_rejection_rolls_back_item_reservation() {
        let admission = Admission::new(2, 4);
        let permit = admission.try_reserve(3).unwrap();
        assert!(admission.try_reserve(2).is_none());
        assert!(admission.try_reserve(usize::MAX).is_none());
        assert_eq!(admission.in_use(), (1, 3));
        let tail = admission
            .try_reserve(1)
            .expect("failed admission did not leak an item");
        assert!(admission.try_reserve(0).is_none());
        drop((permit, tail));
        assert_eq!(admission.in_use(), (0, 0));
    }

    #[test]
    fn response_overflow_drops_whole_responses_and_recovers() {
        let admission = Admission::new(2, 4);
        let first = admission
            .admit_response(Bytes::from_static(b"abc"))
            .unwrap();
        assert!(admission
            .admit_response(Bytes::from_static(b"de"))
            .is_none());
        assert_eq!(admission.in_use(), (1, 3));
        assert_eq!(first.as_ref(), b"abc");
        drop(first);
        let next = admission.admit_response(Bytes::from_static(b"de")).unwrap();
        assert_eq!(next.as_ref(), b"de");
        drop(next);
        assert_eq!(admission.in_use(), (0, 0));
    }

    #[test]
    fn response_overflow_warning_waits_for_full_backlog_recovery() {
        let admission = Admission::new(2, 4);
        let first = admission.admit_response(Bytes::from_static(b"ab")).unwrap();
        let second = admission.admit_response(Bytes::from_static(b"cd")).unwrap();
        assert!(admission.admit_response(Bytes::from_static(b"e")).is_none());
        assert!(admission.warned.load(Ordering::Acquire));

        drop(first);
        assert!(admission.warned.load(Ordering::Acquire));
        assert!(admission
            .admit_response(Bytes::from_static(b"abc"))
            .is_none());
        assert!(admission.warned.load(Ordering::Acquire));

        drop(second);
        assert!(!admission.warned.load(Ordering::Acquire));
    }
}
