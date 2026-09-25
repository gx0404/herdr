//! Read-only, bounded compressed rollout access. The runtime owns this cache;
//! filesystem I/O and decoding never hold its mutex. Metadata survives content eviction.
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;

use super::{Lifecycle, RolloutMeta, SourceError, ThreadOrigin};
use ruzstd::decoding::StreamingDecoder;

const MAX_INPUT: u64 = 16 * 1024 * 1024;
const MAX_OUTPUT: usize = 64 * 1024 * 1024;
const MAX_WINDOW: u64 = 8 * 1024 * 1024;
const MAX_FRAMES: usize = 128;
const MAX_CACHE: usize = 128 * 1024 * 1024;
const MAX_ENTRIES: usize = 2048;

/// The pass budget includes a conservative window charge while a frame is unfinished.
/// This also bounds a decoder's read-ahead while reading only the metadata prefix.
pub(super) struct DecodeBudget {
    remaining: u64,
    input_remaining: u64,
}
impl Default for DecodeBudget {
    fn default() -> Self {
        Self {
            remaining: 72 * 1024 * 1024,
            input_remaining: 64 * 1024 * 1024,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Fingerprint {
    len: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
}
fn fingerprint(path: &Path) -> Result<Fingerprint, SourceError> {
    let meta = fs::metadata(path).map_err(SourceError::Io)?;
    if !meta.is_file() {
        return Err(SourceError::Unsupported);
    }
    Ok(Fingerprint {
        len: meta.len(),
        modified: meta.modified().ok(),
        created: meta.created().ok(),
    })
}

#[derive(Debug, Clone, Copy)]
enum Failure {
    Limit,
    Corrupt,
}
impl Failure {
    fn error(self) -> SourceError {
        match self {
            Self::Limit => SourceError::Unsupported,
            Self::Corrupt => SourceError::Malformed("invalid compressed Codex rollout".into()),
        }
    }
}
struct Entry {
    fingerprint: Fingerprint,
    touched: u64,
    meta: Option<Option<RolloutMeta>>,
    lifecycle: Option<Lifecycle>,
    decoded: Option<Arc<DecodedBytes>>,
    failure: Option<Failure>,
}
impl Entry {
    fn bytes(&self, path: &Path) -> usize {
        let meta_bytes = self
            .meta
            .as_ref()
            .and_then(Option::as_ref)
            .map_or(0, |meta| {
                meta.id.len()
                    + meta.path.as_os_str().as_encoded_bytes().len()
                    + [
                        &meta.parent_id,
                        &meta.nickname,
                        &meta.role,
                        &meta.agent_path,
                    ]
                    .into_iter()
                    .filter_map(Option::as_ref)
                    .map(String::len)
                    .sum::<usize>()
                    + if let ThreadOrigin::Named(name) = &meta.origin {
                        name.len()
                    } else {
                        0
                    }
            });
        let life_bytes = match &self.lifecycle {
            Some(
                Lifecycle::Completed {
                    error: Some(value), ..
                }
                | Lifecycle::Aborted {
                    reason: Some(value),
                    ..
                },
            ) => value.len(),
            _ => 0,
        };
        std::mem::size_of::<Self>()
            + path.as_os_str().as_encoded_bytes().len()
            + meta_bytes
            + life_bytes
            + self.decoded.as_ref().map_or(0, |data| data.capacity())
    }
}

#[derive(Default)]
pub(crate) struct RolloutCache {
    entries: HashMap<PathBuf, Entry>,
    clock: u64,
    bytes: usize,
    in_flight: bool,
    live_bytes: Arc<AtomicUsize>,
    #[cfg(test)]
    loads: usize,
}
impl RolloutCache {
    #[cfg(test)]
    pub(super) fn decode_count(&self) -> usize {
        self.loads
    }
    fn touch(&mut self, path: &Path, fp: &Fingerprint) -> &mut Entry {
        self.clock = self.clock.wrapping_add(1);
        if self
            .entries
            .get(path)
            .is_some_and(|entry| entry.fingerprint != *fp)
        {
            if let Some(old) = self.entries.remove(path) {
                self.bytes -= old.bytes(path);
            }
        }
        let clock = self.clock;
        if !self.entries.contains_key(path) {
            while self.entries.len() >= MAX_ENTRIES {
                let oldest = self
                    .entries
                    .iter()
                    .min_by_key(|(_, entry)| entry.touched)
                    .map(|(key, _)| key.clone());
                let Some(key) = oldest else {
                    break;
                };
                if let Some(entry) = self.entries.remove(&key) {
                    self.bytes -= entry.bytes(&key);
                }
            }
            self.trim(std::mem::size_of::<Entry>() + path.as_os_str().as_encoded_bytes().len());
        }
        let entry = self.entries.entry(path.to_path_buf()).or_insert_with(|| {
            let entry = Entry {
                fingerprint: fp.clone(),
                touched: clock,
                meta: None,
                lifecycle: None,
                decoded: None,
                failure: None,
            };
            self.bytes += entry.bytes(path);
            entry
        });
        entry.touched = clock;
        entry
    }

    fn update(&mut self, path: &Path, fp: &Fingerprint, f: impl FnOnce(&mut Entry)) {
        let before = self.touch(path, fp).bytes(path);
        let entry = self.touch(path, fp);
        f(entry);
        let after = entry.bytes(path);
        self.bytes = self.bytes.saturating_sub(before).saturating_add(after);
        self.trim(0);
    }
    fn trim(&mut self, reserve: usize) {
        while self.bytes.saturating_add(reserve) > MAX_CACHE || self.entries.len() > MAX_ENTRIES {
            // First evict decoded bodies only: unchanged metadata/lifecycle must not be redecoded on every poll.
            let decoded = self
                .entries
                .iter()
                .filter(|(_, entry)| {
                    entry
                        .decoded
                        .as_ref()
                        .is_some_and(|data| Arc::strong_count(data) == 1)
                })
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(key, _)| key.clone());
            if let Some(key) = decoded {
                if let Some(entry) = self.entries.get_mut(&key) {
                    if let Some(bytes) = entry.decoded.take() {
                        self.bytes -= bytes.capacity();
                    }
                }
                continue;
            }
            let oldest = self
                .entries
                .iter()
                .filter(|(_, entry)| entry.decoded.is_none())
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(key, _)| key.clone());
            let Some(key) = oldest else {
                break;
            };
            if let Some(entry) = self.entries.remove(&key) {
                self.bytes -= entry.bytes(&key);
            }
        }
    }
}
fn lock(cache: &Mutex<RolloutCache>) -> Result<MutexGuard<'_, RolloutCache>, SourceError> {
    cache.lock().map_err(|_| SourceError::Unavailable)
}
struct Permit<'a>(&'a Mutex<RolloutCache>);
impl Drop for Permit<'_> {
    fn drop(&mut self) {
        if let Ok(mut cache) = self.0.lock() {
            cache.in_flight = false;
        }
    }
}
fn permit(cache: &Mutex<RolloutCache>, reserve: usize) -> Result<Permit<'_>, SourceError> {
    let mut state = lock(cache)?;
    if state.in_flight {
        return Err(SourceError::Unavailable);
    }
    state.trim(reserve);
    let cached_bodies: usize = state
        .entries
        .values()
        .filter_map(|entry| entry.decoded.as_ref())
        .map(|data| data.capacity())
        .sum();
    let total = state
        .bytes
        .saturating_sub(cached_bodies)
        .saturating_add(state.live_bytes.load(Ordering::Relaxed));
    if total.saturating_add(reserve) > MAX_CACHE {
        return Err(SourceError::Unavailable);
    }
    state.in_flight = true;
    #[cfg(test)]
    {
        state.loads += 1;
    }
    Ok(Permit(cache))
}

/// Count allocated bodies until the last reader releases them, including replaced/evicted entries.
struct DecodedBytes {
    bytes: Vec<u8>,
    live_bytes: Arc<AtomicUsize>,
}
impl std::ops::Deref for DecodedBytes {
    type Target = Vec<u8>;
    fn deref(&self) -> &Self::Target {
        &self.bytes
    }
}
impl Drop for DecodedBytes {
    fn drop(&mut self) {
        self.live_bytes
            .fetch_sub(self.bytes.capacity(), Ordering::Relaxed);
    }
}
#[derive(Clone)]
pub(super) struct SharedBytes(Arc<DecodedBytes>);
impl AsRef<[u8]> for SharedBytes {
    fn as_ref(&self) -> &[u8] {
        self.0.as_slice()
    }
}
pub(super) enum RolloutReader {
    Plain(File),
    Decoded(Cursor<SharedBytes>),
}
impl RolloutReader {
    pub(super) fn len(&self) -> io::Result<u64> {
        match self {
            Self::Plain(file) => Ok(file.metadata()?.len()),
            Self::Decoded(cursor) => Ok(cursor.get_ref().as_ref().len() as u64),
        }
    }
}
impl Read for RolloutReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(file) => file.read(bytes),
            Self::Decoded(cursor) => cursor.read(bytes),
        }
    }
}
impl Seek for RolloutReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        match self {
            Self::Plain(file) => file.seek(pos),
            Self::Decoded(cursor) => cursor.seek(pos),
        }
    }
}
fn compressed(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension == "zst")
}

/// Cold reads are limited to one decoder across both workers. A competing request retries later.
pub(super) fn open(
    path: &Path,
    cache: &Mutex<RolloutCache>,
    budget: &mut DecodeBudget,
) -> Result<RolloutReader, SourceError> {
    // Codex may atomically replace plain storage with .zst (or materialize it back).
    // Resolve the sibling once; do not recurse while a writer is switching representations.
    let alternate;
    let path = if !path.try_exists().map_err(SourceError::Io)? {
        alternate = if compressed(path) {
            path.with_extension("")
        } else {
            {
                let mut value = path.as_os_str().to_os_string();
                value.push(".zst");
                PathBuf::from(value)
            }
        };
        &alternate
    } else {
        path
    };
    if !compressed(path) {
        fingerprint(path)?;
        let file = File::open(path).map_err(SourceError::Io)?;
        if !file.metadata().map_err(SourceError::Io)?.is_file() {
            return Err(SourceError::Unsupported);
        }
        return Ok(RolloutReader::Plain(file));
    }
    let fp = fingerprint(path)?;
    {
        let mut state = lock(cache)?;
        let entry = state.touch(path, &fp);
        if let Some(data) = &entry.decoded {
            return Ok(RolloutReader::Decoded(Cursor::new(SharedBytes(
                data.clone(),
            ))));
        }
        if let Some(failure) = entry.failure {
            return Err(failure.error());
        }
    }
    let _permit = permit(cache, MAX_OUTPUT + MAX_WINDOW as usize)?;
    let result = decode(path, &fp, MAX_OUTPUT, false, budget);
    if fingerprint(path)? != fp {
        return Err(SourceError::Unavailable);
    }
    match result {
        Ok(data) => {
            let live_bytes = lock(cache)?.live_bytes.clone();
            live_bytes.fetch_add(data.capacity(), Ordering::Relaxed);
            let data = Arc::new(DecodedBytes {
                bytes: data,
                live_bytes,
            });
            lock(cache)?.update(path, &fp, |entry| entry.decoded = Some(data.clone()));
            Ok(RolloutReader::Decoded(Cursor::new(SharedBytes(data))))
        }
        Err(DecodeFailure::Permanent(failure)) => {
            lock(cache)?.update(path, &fp, |entry| entry.failure = Some(failure));
            Err(failure.error())
        }
        Err(DecodeFailure::Budget) => Err(SourceError::Unavailable),
        Err(DecodeFailure::Io(error)) => Err(SourceError::Io(error)),
    }
}

pub(super) fn meta(
    path: &Path,
    cache: &Mutex<RolloutCache>,
    budget: &mut DecodeBudget,
) -> Option<RolloutMeta> {
    if !compressed(path) {
        return super::read_meta(path);
    }
    let fp = fingerprint(path).ok()?;
    {
        let mut state = lock(cache).ok()?;
        let entry = state.touch(path, &fp);
        if let Some(meta) = &entry.meta {
            return meta.clone();
        }
    }
    let _permit = permit(
        cache,
        super::MAX_META_LINE_BYTES as usize + MAX_WINDOW as usize,
    )
    .ok()?;
    let result = decode(path, &fp, super::MAX_META_LINE_BYTES as usize, true, budget);
    if fingerprint(path).ok()? != fp {
        return None;
    }
    let meta = match result {
        Ok(bytes) => super::read_meta_from_reader(BufReader::new(bytes.as_slice()), path),
        Err(DecodeFailure::Permanent(_)) => None,
        Err(DecodeFailure::Budget | DecodeFailure::Io(_)) => return None,
    };
    lock(cache)
        .ok()?
        .update(path, &fp, |entry| entry.meta = Some(meta.clone()));
    meta
}

pub(super) fn lifecycle(
    path: &Path,
    cache: &Mutex<RolloutCache>,
    budget: &mut DecodeBudget,
) -> Result<Lifecycle, SourceError> {
    if !compressed(path) {
        return super::lifecycle_from_tail(path, super::STATUS_TAIL_BYTES).map_err(SourceError::Io);
    }
    let fp = fingerprint(path)?;
    if let Some(lifecycle) = &lock(cache)?.touch(path, &fp).lifecycle {
        return Ok(lifecycle.clone());
    }
    let mut reader = open(path, cache, budget)?;
    let len = reader.len().map_err(SourceError::Io)?;
    let life = super::lifecycle_from_reader(&mut reader, len, super::STATUS_TAIL_BYTES)
        .map_err(SourceError::Io)?;
    if fingerprint(path)? == fp {
        lock(cache)?.update(path, &fp, |entry| entry.lifecycle = Some(life.clone()));
    }
    Ok(life)
}

#[derive(Debug)]
enum DecodeFailure {
    Io(io::Error),
    Permanent(Failure),
    Budget,
}
impl From<io::Error> for DecodeFailure {
    fn from(_: io::Error) -> Self {
        Self::Permanent(Failure::Corrupt)
    }
}

/// Decode finite concatenated frames, including bounded skippable frames. Prefix reads stop
/// without checking an unfinished frame's checksum; full content checks every complete frame.
fn decode(
    path: &Path,
    fp: &Fingerprint,
    limit: usize,
    prefix: bool,
    budget: &mut DecodeBudget,
) -> Result<Vec<u8>, DecodeFailure> {
    if fp.len > MAX_INPUT {
        return Err(DecodeFailure::Permanent(Failure::Limit));
    }
    if fp.len > budget.input_remaining {
        return Err(DecodeFailure::Budget);
    }
    budget.input_remaining -= fp.len;
    let file = File::open(path).map_err(DecodeFailure::Io)?;
    decode_reader(file.take(fp.len), limit, prefix, budget)
}

/// Preserve underlying file errors even when the decoder wraps them as format errors.
struct ReadFailure<'a, R> {
    reader: R,
    error: &'a mut Option<io::Error>,
}
impl<R: Read> Read for ReadFailure<'_, R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.reader.read(bytes).inspect_err(|error| {
            *self.error = Some(io::Error::new(error.kind(), error.to_string()));
        })
    }
}
fn decode_reader(
    reader: impl Read,
    limit: usize,
    prefix: bool,
    budget: &mut DecodeBudget,
) -> Result<Vec<u8>, DecodeFailure> {
    let mut error = None;
    let result = decode_frames(
        BufReader::new(ReadFailure {
            reader,
            error: &mut error,
        }),
        limit,
        prefix,
        budget,
    );
    if result.is_err() {
        if let Some(error) = error {
            return Err(DecodeFailure::Io(error));
        }
    }
    result
}
fn decode_frames(
    mut source: impl BufRead,
    limit: usize,
    prefix: bool,
    budget: &mut DecodeBudget,
) -> Result<Vec<u8>, DecodeFailure> {
    let mut output = Vec::new();
    let mut frames = 0;
    loop {
        if source.fill_buf()?.is_empty() {
            return Ok(output);
        }
        frames += 1;
        if frames > MAX_FRAMES {
            return Err(DecodeFailure::Permanent(Failure::Limit));
        }
        if budget.remaining < MAX_WINDOW {
            return Err(DecodeFailure::Budget);
        }
        budget.remaining -= MAX_WINDOW;
        // Read magic explicitly so skippable frames cannot be mistaken for successful EOF.
        let mut magic = [0; 4];
        source.read_exact(&mut magic)?;
        let magic_value = u32::from_le_bytes(magic);
        if (0x184d2a50..=0x184d2a5f).contains(&magic_value) {
            let mut length = [0; 4];
            source.read_exact(&mut length)?;
            let length = u32::from_le_bytes(length) as u64;
            if length > MAX_INPUT {
                return Err(DecodeFailure::Permanent(Failure::Limit));
            }
            if io::copy(&mut source.by_ref().take(length), &mut io::sink())? != length {
                return Err(DecodeFailure::Permanent(Failure::Corrupt));
            }
            budget.remaining += MAX_WINDOW;
            continue;
        }
        let chained = Cursor::new(magic).chain(&mut source);
        let mut decoder =
            StreamingDecoder::new_with_max_window_size(chained, MAX_WINDOW).map_err(|error| {
                DecodeFailure::Permanent(match error {
                    ruzstd::decoding::errors::FrameDecoderError::WindowSizeTooBig { .. } => {
                        Failure::Limit
                    }
                    _ => Failure::Corrupt,
                })
            })?;
        let content_size = decoder.decoder.content_size();
        if !prefix && content_size > limit.saturating_sub(output.len()) as u64 {
            return Err(DecodeFailure::Permanent(Failure::Limit));
        }
        let mut buffer = [0; 8192];
        loop {
            if decoder.decoder.is_finished() && decoder.decoder.can_collect() == 0 {
                break;
            }
            if prefix && output.len() == limit {
                return Ok(output);
            }
            if budget.remaining == 0 {
                return Err(DecodeFailure::Budget);
            }
            let remaining = limit.saturating_sub(output.len());
            let ask = buffer
                .len()
                .min(if prefix {
                    remaining
                } else {
                    remaining.saturating_add(1)
                })
                .min(budget.remaining as usize);
            let read = decoder.read(&mut buffer[..ask])?;
            if read == 0 {
                break;
            }
            budget.remaining -= read as u64;
            if read > remaining {
                return Err(DecodeFailure::Permanent(Failure::Limit));
            }
            // Avoid geometric growth allocating beyond the advertised per-file limit.
            if output.len() + read > output.capacity() {
                let capacity = (output.capacity().max(8192) * 2)
                    .min(limit)
                    .max(output.len() + read);
                output.reserve_exact(capacity - output.len());
            }
            output.extend_from_slice(&buffer[..read]);
        }
        if let Some(expected) = decoder.decoder.get_checksum_from_data() {
            if decoder.decoder.get_calculated_checksum() != Some(expected) {
                return Err(DecodeFailure::Permanent(Failure::Corrupt));
            }
        }
        budget.remaining += MAX_WINDOW; // A complete frame's bytes have all been accounted for.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruzstd::encoding::{compress_to_vec, CompressionLevel};

    fn source(name: &str, bytes: &[u8]) -> (PathBuf, PathBuf) {
        let dir = super::super::tests::unique_temp_home(name);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout.jsonl.zst");
        fs::write(&path, bytes).unwrap();
        (dir, path)
    }
    fn encoded(bytes: &[u8]) -> Vec<u8> {
        compress_to_vec(bytes, CompressionLevel::Fastest)
    }
    fn content(path: &Path, cache: &Mutex<RolloutCache>) -> Result<Vec<u8>, SourceError> {
        let mut reader = open(path, cache, &mut DecodeBudget::default())?;
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).map_err(SourceError::Io)?;
        Ok(bytes)
    }

    #[test]
    fn transient_reader_errors_are_not_format_failures() {
        struct FailsAfter {
            bytes: Cursor<Vec<u8>>,
            allowed: usize,
            kind: io::ErrorKind,
        }
        impl Read for FailsAfter {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                if self.allowed == 0 {
                    return Err(io::Error::new(self.kind, "temporary sharing lock"));
                }
                let count = bytes.len().min(self.allowed);
                let read = self.bytes.read(&mut bytes[..count])?;
                self.allowed -= read;
                Ok(read)
            }
        }
        for allowed in [0, 6, 10] {
            let reader = FailsAfter {
                bytes: Cursor::new(encoded(b"valid content")),
                allowed,
                kind: io::ErrorKind::PermissionDenied,
            };
            assert!(
                matches!(decode_reader(reader, MAX_OUTPUT, false, &mut DecodeBudget::default()), Err(DecodeFailure::Io(error)) if error.kind() == io::ErrorKind::PermissionDenied)
            );
        }
        let reader = FailsAfter {
            bytes: Cursor::new(encoded(b"valid content")),
            allowed: 0,
            kind: io::ErrorKind::Interrupted,
        };
        assert!(
            matches!(decode_reader(reader, MAX_OUTPUT, false, &mut DecodeBudget::default()), Err(DecodeFailure::Io(error)) if error.kind() == io::ErrorKind::Interrupted)
        );
        assert_eq!(
            decode_reader(
                Cursor::new(encoded(b"valid content")),
                MAX_OUTPUT,
                false,
                &mut DecodeBudget::default()
            )
            .unwrap(),
            b"valid content"
        );
    }

    #[test]
    fn compressed_completed_thread_becomes_running_on_followup() {
        let completed = b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\"}}\n";
        let (dir, path) = source("compressed-followup", &encoded(completed));
        let cache = Mutex::new(RolloutCache::default());
        assert!(matches!(
            lifecycle(&path, &cache, &mut DecodeBudget::default()).unwrap(),
            Lifecycle::Completed { .. }
        ));
        let mut next = completed.to_vec();
        next.extend(b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n");
        fs::write(&path, encoded(&next)).unwrap();
        assert!(matches!(
            lifecycle(&path, &cache, &mut DecodeBudget::default()).unwrap(),
            Lifecycle::Running
        ));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn concatenated_frames_skips_checksums_and_failure_cache() {
        let mut bytes = encoded(b"first\n");
        bytes.extend(0x184d2a50u32.to_le_bytes());
        bytes.extend(3u32.to_le_bytes());
        bytes.extend(b"abc");
        bytes.extend(encoded(b"second\n"));
        let (dir, path) = source("frames", &bytes);
        let cache = Mutex::new(RolloutCache::default());
        assert_eq!(content(&path, &cache).unwrap(), b"first\nsecond\n");
        assert_eq!(content(&path, &cache).unwrap(), b"first\nsecond\n");
        assert_eq!(cache.lock().unwrap().loads, 1);
        *bytes.last_mut().unwrap() ^= 1;
        fs::write(&path, &bytes).unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH)
            .unwrap();
        assert!(matches!(
            content(&path, &cache),
            Err(SourceError::Malformed(_))
        ));
        assert!(matches!(
            content(&path, &cache),
            Err(SourceError::Malformed(_))
        ));
        assert_eq!(cache.lock().unwrap().loads, 2);
        fs::write(&path, encoded(b"replacement\n")).unwrap();
        assert_eq!(content(&path, &cache).unwrap(), b"replacement\n");
        assert_eq!(cache.lock().unwrap().loads, 3);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn decode_bounds_output_input_frames_and_window() {
        let (dir, path) = source("limits", &encoded(&vec![b'x'; 4096]));
        let fp = fingerprint(&path).unwrap();
        assert!(matches!(
            decode(&path, &fp, 100, false, &mut DecodeBudget::default()),
            Err(DecodeFailure::Permanent(Failure::Limit))
        ));
        assert_eq!(
            decode(&path, &fp, 100, true, &mut DecodeBudget::default()).unwrap(),
            vec![b'x'; 100]
        );
        fs::write(&path, encoded(&[b'x'; 100])).unwrap();
        let exact_fp = fingerprint(&path).unwrap();
        let mut exact_budget = DecodeBudget {
            remaining: MAX_WINDOW + 100,
            input_remaining: MAX_INPUT,
        };
        assert_eq!(
            decode(&path, &exact_fp, 100, false, &mut exact_budget).unwrap(),
            [b'x'; 100]
        );
        let mut budget = DecodeBudget {
            remaining: MAX_WINDOW - 1,
            input_remaining: MAX_INPUT,
        };
        assert!(matches!(
            decode(&path, &fp, MAX_OUTPUT, false, &mut budget),
            Err(DecodeFailure::Budget)
        ));
        let mut budget = DecodeBudget {
            remaining: MAX_OUTPUT as u64,
            input_remaining: 0,
        };
        assert!(matches!(
            decode(&path, &fp, MAX_OUTPUT, false, &mut budget),
            Err(DecodeFailure::Budget)
        ));
        // No payload is necessary to reject a declared 16 MiB decoder window.
        fs::write(&path, [0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x70]).unwrap();
        assert!(content(&path, &Mutex::new(RolloutCache::default())).is_err());
        let skips = [0x50, 0x2a, 0x4d, 0x18, 0, 0, 0, 0].repeat(MAX_FRAMES + 1);
        fs::write(&path, skips).unwrap();
        assert!(matches!(
            content(&path, &Mutex::new(RolloutCache::default())),
            Err(SourceError::Unsupported)
        ));
        File::create(&path).unwrap().set_len(MAX_INPUT + 1).unwrap();
        let cache = Mutex::new(RolloutCache::default());
        assert!(matches!(
            content(&path, &cache),
            Err(SourceError::Unsupported)
        ));
        assert!(matches!(
            content(&path, &cache),
            Err(SourceError::Unsupported)
        ));
        assert_eq!(cache.lock().unwrap().loads, 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn workers_share_one_decoder_and_retry_transient_budget() {
        let (dir, path) = source("single-flight", &encoded(b"ok"));
        let cache = Mutex::new(RolloutCache::default());
        let guard = permit(&cache, MAX_OUTPUT).unwrap();
        assert!(matches!(
            content(&path, &cache),
            Err(SourceError::Unavailable)
        ));
        drop(guard);
        let mut exhausted = DecodeBudget {
            remaining: 0,
            input_remaining: 0,
        };
        assert!(matches!(
            open(&path, &cache, &mut exhausted),
            Err(SourceError::Unavailable)
        ));
        assert_eq!(content(&path, &cache).unwrap(), b"ok");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn metadata_and_lifecycle_survive_body_eviction_and_replace_together() {
        let json = b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"01a0c600-0000-7000-8000-000000000001\",\"source\":\"cli\"}}\n{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\"}}\n";
        let (dir, path) = source("meta-cache", &encoded(json));
        let cache = Mutex::new(RolloutCache::default());
        assert!(meta(&path, &cache, &mut DecodeBudget::default()).is_some());
        lifecycle(&path, &cache, &mut DecodeBudget::default()).unwrap();
        let loads = cache.lock().unwrap().loads;
        let reserve = MAX_CACHE - cache.lock().unwrap().bytes + 1;
        cache.lock().unwrap().trim(reserve);
        assert!(cache.lock().unwrap().entries[&path].decoded.is_none());
        assert!(meta(&path, &cache, &mut DecodeBudget::default()).is_some());
        lifecycle(&path, &cache, &mut DecodeBudget::default()).unwrap();
        assert_eq!(cache.lock().unwrap().loads, loads);
        fs::write(&path, encoded(b"not json\n")).unwrap();
        assert!(meta(&path, &cache, &mut DecodeBudget::default()).is_none());
        assert!(meta(&path, &cache, &mut DecodeBudget::default()).is_none());
        assert_eq!(cache.lock().unwrap().loads, loads + 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn replaced_live_readers_still_count_against_the_shared_budget() {
        let (dir, path) = source("live-replacement", &encoded(b"first"));
        let cache = Mutex::new(RolloutCache::default());
        let reader = open(&path, &cache, &mut DecodeBudget::default()).unwrap();
        let old_bytes = cache.lock().unwrap().live_bytes.load(Ordering::Relaxed);
        fs::write(&path, encoded(b"a longer replacement")).unwrap();
        let new_reader = open(&path, &cache, &mut DecodeBudget::default()).unwrap();
        let total = cache.lock().unwrap().live_bytes.load(Ordering::Relaxed);
        assert!(total > old_bytes);
        drop(reader);
        assert_eq!(
            cache.lock().unwrap().live_bytes.load(Ordering::Relaxed),
            total - old_bytes
        );
        drop(new_reader);
        cache.lock().unwrap().trim(MAX_CACHE);
        assert_eq!(cache.lock().unwrap().live_bytes.load(Ordering::Relaxed), 0);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cache_entry_budget_and_live_reader_memory_remain_accounted() {
        let (dir, path) = source("cache-bounds", &encoded(b"live"));
        let cache = Mutex::new(RolloutCache::default());
        let reader = open(&path, &cache, &mut DecodeBudget::default()).unwrap();
        cache.lock().unwrap().trim(MAX_CACHE);
        assert!(cache.lock().unwrap().bytes >= 4);
        drop(reader);
        cache.lock().unwrap().trim(MAX_CACHE);
        assert_eq!(cache.lock().unwrap().bytes, 0);
        let fp = fingerprint(&path).unwrap();
        for index in 0..MAX_ENTRIES + 20 {
            cache
                .lock()
                .unwrap()
                .touch(&dir.join(index.to_string()), &fp);
        }
        assert_eq!(cache.lock().unwrap().entries.len(), MAX_ENTRIES);
        assert!(cache.lock().unwrap().bytes <= MAX_CACHE);
        fs::remove_dir_all(dir).unwrap();
    }
}
