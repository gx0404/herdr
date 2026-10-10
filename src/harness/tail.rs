//! 有界尾读与按文件指纹缓存派生值（推广 codex `RolloutCache` 的思路）：会话文件只读
//! 末尾一个有界窗口；由它派生的结果按（路径，长度，修改时刻）缓存，文件没变就不再读。
//! I/O 与派生都不持有缓存锁，锁只在查与存的瞬间持有。

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

/// [`TailCache`] 缺省的容量（项数）。每项是一个 pane 会话的派生摘要，几百字节。
pub const DEFAULT_CAPACITY: usize = 256;

/// 文件指纹：长度与修改时刻。追加写入改长度，原地重写改修改时刻。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint {
    len: u64,
    modified: Option<SystemTime>,
}

impl Fingerprint {
    /// 普通文件的指纹；不存在或不是普通文件时为 `None`。
    pub fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        meta.is_file().then(|| Self {
            len: meta.len(),
            modified: meta.modified().ok(),
        })
    }
}

/// 一组输入文件的联合指纹；某个文件出现或消失也算变化。
pub type Stamp = Vec<Option<Fingerprint>>;

/// 按顺序取 `paths` 的指纹。
pub fn stamp(paths: &[&Path]) -> Stamp {
    paths.iter().map(|path| Fingerprint::of(path)).collect()
}

/// 文件末尾的一个有界窗口。
pub struct Tail {
    bytes: Vec<u8>,
    /// 窗口从文件中间开始：第一段是被截断的半行（或上一行的换行符），读行时丢掉。
    cut: bool,
}

impl Tail {
    /// 读 `path` 的最后 `max_bytes` 字节。
    pub fn read(path: &Path, max_bytes: u64) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let len = file.metadata()?.len();
        Self::read_from(&mut file, len, max_bytes)
    }

    /// 从总长 `len` 的可定位读取器读最后 `max_bytes` 字节。窗口不从开头起时多读前一个
    /// 字节再丢掉第一段：起点落在行中间时丢的是半行，恰好落在行首时丢的只是上一行的
    /// 换行符，完整的行一条不丢。
    pub fn read_from(
        reader: &mut (impl Read + Seek),
        len: u64,
        max_bytes: u64,
    ) -> io::Result<Self> {
        let start = len.saturating_sub(max_bytes);
        let cut = start > 0;
        let from = start - u64::from(cut);
        reader.seek(SeekFrom::Start(from))?;
        let want = len - from;
        let mut bytes = Vec::with_capacity(usize::try_from(want).unwrap_or(0));
        reader.take(want).read_to_end(&mut bytes)?;
        Ok(Self { bytes, cut })
    }

    /// 整份文件都在窗口里。
    pub fn whole_file(&self) -> bool {
        !self.cut
    }

    /// 窗口里的完整行，去掉首尾 ASCII 空白、跳过空行，按文件顺序。末尾没写完的半行
    /// 原样给出，调用方解析失败即跳过。
    pub fn lines(&self) -> impl DoubleEndedIterator<Item = &[u8]> {
        let mut window = self.bytes.as_slice();
        if self.cut {
            window = match window.iter().position(|byte| *byte == b'\n') {
                Some(newline) => &window[newline + 1..],
                None => &[],
            };
        }
        window
            .split(|byte| *byte == b'\n')
            .map(<[u8]>::trim_ascii)
            .filter(|line| !line.is_empty())
    }
}

/// 按键缓存派生值，键对应的输入文件指纹变了即作废；最多 `capacity` 项，满了丢最久
/// 没用的一项。
pub struct TailCache<V> {
    entries: HashMap<PathBuf, CacheEntry<V>>,
    clock: u64,
    capacity: usize,
}

struct CacheEntry<V> {
    stamp: Stamp,
    touched: u64,
    value: V,
}

impl<V> Default for TailCache<V> {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }
}

impl<V> TailCache<V> {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            clock: 0,
            capacity: capacity.max(1),
        }
    }
}

impl<V: Clone> TailCache<V> {
    /// 指纹与存入时相同才命中。
    pub fn get(&mut self, key: &Path, stamp: &Stamp) -> Option<V> {
        self.clock = self.clock.wrapping_add(1);
        let clock = self.clock;
        let entry = self.entries.get_mut(key)?;
        if entry.stamp != *stamp {
            return None;
        }
        entry.touched = clock;
        Some(entry.value.clone())
    }

    pub fn insert(&mut self, key: PathBuf, stamp: Stamp, value: V) {
        self.clock = self.clock.wrapping_add(1);
        if !self.entries.contains_key(&key) {
            while self.entries.len() >= self.capacity {
                let Some(oldest) = self
                    .entries
                    .iter()
                    .min_by_key(|(_, entry)| entry.touched)
                    .map(|(key, _)| key.clone())
                else {
                    break;
                };
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(
            key,
            CacheEntry {
                stamp,
                touched: self.clock,
                value,
            },
        );
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// 带缓存地派生：`inputs` 的联合指纹与上次相同就直接用缓存；派生期间输入又变了就不
/// 存（下一次重读）。没有缓存时直接派生。缓存锁中毒时当作没有缓存。
pub fn cached<V: Clone>(
    cache: Option<&Mutex<TailCache<V>>>,
    key: &Path,
    inputs: &[&Path],
    derive: impl FnOnce() -> V,
) -> V {
    match try_cached(cache, key, inputs, || {
        Ok::<V, std::convert::Infallible>(derive())
    }) {
        Ok(value) => value,
        Err(never) => match never {},
    }
}

/// 同 [`cached`]，派生可能失败：失败不进缓存，下一次照常重试。
pub fn try_cached<V: Clone, E>(
    cache: Option<&Mutex<TailCache<V>>>,
    key: &Path,
    inputs: &[&Path],
    derive: impl FnOnce() -> Result<V, E>,
) -> Result<V, E> {
    let Some(cache) = cache else {
        return derive();
    };
    let before = stamp(inputs);
    if let Some(hit) = cache
        .lock()
        .ok()
        .and_then(|mut cache| cache.get(key, &before))
    {
        return Ok(hit);
    }
    let value = derive()?;
    if stamp(inputs) == before {
        if let Ok(mut cache) = cache.lock() {
            cache.insert(key.to_path_buf(), before, value.clone());
        }
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn lines(tail: &Tail) -> Vec<String> {
        tail.lines()
            .map(|line| String::from_utf8_lossy(line).into_owned())
            .collect()
    }

    #[test]
    fn a_window_inside_the_file_drops_only_the_cut_line() {
        let text = b"first\nsecond\nthird\n";
        let len = text.len() as u64;
        // 起点落在 `second` 中间：半行丢掉。
        let tail = Tail::read_from(&mut Cursor::new(text), len, 9).unwrap();
        assert_eq!(lines(&tail), ["third"]);
        assert!(!tail.whole_file());
        // 起点恰在 `second` 行首：一条完整行都不丢。
        let tail = Tail::read_from(&mut Cursor::new(text), len, 13).unwrap();
        assert_eq!(lines(&tail), ["second", "third"]);
        // 窗口覆盖整份文件。
        let tail = Tail::read_from(&mut Cursor::new(text), len, 1 << 20).unwrap();
        assert_eq!(lines(&tail), ["first", "second", "third"]);
        assert!(tail.whole_file());
        assert_eq!(
            lines(&tail).into_iter().rev().collect::<Vec<_>>(),
            ["third", "second", "first"],
            "可以倒着读"
        );
    }

    #[test]
    fn blank_lines_and_an_unfinished_last_line_are_handled() {
        let text = b"a\r\n\n  \nb\n{\"half\":";
        let tail = Tail::read_from(&mut Cursor::new(text), text.len() as u64, 1 << 20).unwrap();
        assert_eq!(lines(&tail), ["a", "b", "{\"half\":"]);
        let tail = Tail::read_from(&mut Cursor::new(b""), 0, 64).unwrap();
        assert_eq!(lines(&tail), Vec::<String>::new());
        let tail = Tail::read_from(&mut Cursor::new(b"no newline at all"), 17, 4).unwrap();
        assert_eq!(lines(&tail), Vec::<String>::new(), "整个窗口都是半行");
    }

    #[test]
    fn the_cache_hits_only_while_the_stamp_is_unchanged() {
        let dir = crate::config::test_dirs::TempDir::new("harness-tail-cache");
        let file = dir.path().join("session.jsonl");
        std::fs::write(&file, "one\n").unwrap();
        let cache = Mutex::new(TailCache::<usize>::default());
        let mut reads = 0;
        let read = |reads: &mut usize| {
            cached(Some(&cache), &file, &[&file], || {
                *reads += 1;
                std::fs::read_to_string(&file).unwrap().lines().count()
            })
        };
        assert_eq!(read(&mut reads), 1);
        assert_eq!(read(&mut reads), 1);
        assert_eq!(reads, 1, "文件没变不再派生");
        std::fs::write(&file, "one\ntwo\n").unwrap();
        assert_eq!(read(&mut reads), 2);
        assert_eq!(reads, 2, "长度变了重新派生");
        std::fs::remove_file(&file).unwrap();
        assert_eq!(
            cached(Some(&cache), &file, &[&file], || 0),
            0,
            "文件消失也算变化"
        );
        assert_eq!(cached(None, &file, &[&file], || 7), 7, "没有缓存直接派生");

        std::fs::write(&file, "one\n").unwrap();
        let failed: Result<usize, &str> = try_cached(Some(&cache), &file, &[&file], || Err("busy"));
        assert_eq!(failed, Err("busy"));
        assert_eq!(
            try_cached(Some(&cache), &file, &[&file], || Ok::<_, &str>(9)),
            Ok(9),
            "失败不进缓存，下一次照常派生"
        );
        assert_eq!(
            try_cached(Some(&cache), &file, &[&file], || Err::<usize, _>("busy")),
            Ok(9),
            "成功的结果命中缓存"
        );
    }

    #[test]
    fn the_cache_evicts_the_least_recently_used_entry() {
        let mut cache = TailCache::with_capacity(2);
        let stamp: Stamp = vec![None];
        cache.insert(PathBuf::from("a"), stamp.clone(), 1);
        cache.insert(PathBuf::from("b"), stamp.clone(), 2);
        assert_eq!(cache.get(Path::new("a"), &stamp), Some(1));
        cache.insert(PathBuf::from("c"), stamp.clone(), 3);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get(Path::new("b"), &stamp), None, "最久没用的被淘汰");
        assert_eq!(cache.get(Path::new("a"), &stamp), Some(1));
        assert_eq!(cache.get(Path::new("c"), &stamp), Some(3));
        assert_eq!(
            cache.get(
                Path::new("c"),
                &vec![Some(Fingerprint {
                    len: 1,
                    modified: None
                })]
            ),
            None,
            "指纹不同不命中"
        );
    }
}
