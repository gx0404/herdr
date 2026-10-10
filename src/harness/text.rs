//! harness 适配器共用的文本工具：压空白、按字符 / 字节截断、单行清洗、去 markdown。
//! 各适配器原先各写一份（`collapse` / `clip_chars` / `clip_bytes` / `clean_line`），
//! 语义在这里统一。

/// 把连续空白与控制字符压成一个空格，去掉首尾空白。
pub fn collapse(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() || ch.is_control() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch);
    }
    out
}

/// 取前 `max_chars` 个字符，超出时追加省略号（结果最多 `max_chars + 1` 个字符）。
#[allow(dead_code)] // 会话洞察适配器（阶段 2b）接入
pub fn clip_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let mut out: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

/// 截到不超过 `max_chars` 个字符（含省略号）。
pub fn fit_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    if max_chars == 0 {
        return String::new();
    }
    let mut out: String = text.chars().take(max_chars - 1).collect();
    out.push('…');
    out
}

/// 按字节上限截断到字符边界。
#[allow(dead_code)] // 会话洞察适配器（阶段 2b）接入
pub fn clip_bytes(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// 单行展示文本：压空白后截到 `max_chars` 个字符（含省略号）；空文本为 `None`。
pub fn clean_line(text: &str, max_chars: usize) -> Option<String> {
    let line = collapse(text);
    (!line.is_empty()).then(|| fit_chars(&line, max_chars))
}

/// 千位以上的计数缩写：`950` / `1.2k` / `3.4M`。
#[allow(dead_code)] // 会话洞察适配器（阶段 2b）接入
pub fn compact_count(value: u64) -> String {
    if value < 1_000 {
        value.to_string()
    } else if value < 1_000_000 {
        format!("{}.{}k", value / 1_000, (value % 1_000) / 100)
    } else {
        format!("{}.{}M", value / 1_000_000, (value % 1_000_000) / 100_000)
    }
}

/// 去掉常见 markdown 记号，留下可读正文（通知与摘要用）：代码围栏、标题井号、
/// 列表 / 引用前缀、强调与行内代码记号、链接与图片只留文字。
pub fn strip_markdown(text: &str) -> String {
    let mut lines = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            continue;
        }
        let line = line.trim_start_matches('#').trim_start();
        let line = line.trim_start_matches('>').trim_start();
        let line = ["- [ ] ", "- [x] ", "- ", "* ", "+ "]
            .iter()
            .find_map(|prefix| line.strip_prefix(prefix))
            .unwrap_or(line);
        lines.push(strip_inline_markdown(line));
    }
    collapse(&lines.join(" "))
}

fn strip_inline_markdown(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let chars = line.chars().collect::<Vec<_>>();
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        match ch {
            // `_` 不当强调记号去掉：摘要里常有 `focused_pane_id` 这类标识符。
            '*' | '`' => {
                index += 1;
            }
            '~' if chars.get(index + 1) == Some(&'~') => {
                index += 2;
            }
            '!' if chars.get(index + 1) == Some(&'[') => {
                index += 1;
            }
            '[' => {
                // `[文字](链接)` → 文字；不成对时原样保留。
                if let Some(close) = chars[index + 1..].iter().position(|&c| c == ']') {
                    let close = index + 1 + close;
                    if chars.get(close + 1) == Some(&'(') {
                        if let Some(end) = chars[close + 2..].iter().position(|&c| c == ')') {
                            out.extend(&chars[index + 1..close]);
                            index = close + 2 + end + 1;
                            continue;
                        }
                    }
                }
                out.push(ch);
                index += 1;
            }
            _ => {
                out.push(ch);
                index += 1;
            }
        }
    }
    out
}

/// 通知 / 摘要正文：去 markdown、压成一行、截到 `max_chars`（含省略号）。
pub fn summary_text(text: &str, max_chars: usize) -> Option<String> {
    clean_line(&strip_markdown(text), max_chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapse_squeezes_whitespace_and_controls() {
        assert_eq!(collapse("  a \n\t b\u{7}c  "), "a b c");
        assert_eq!(collapse(""), "");
    }

    #[test]
    fn clip_and_fit_differ_in_ellipsis_budget() {
        assert_eq!(clip_chars("abcdef", 3), "abc…");
        assert_eq!(clip_chars("abc", 3), "abc");
        assert_eq!(fit_chars("abcdef", 3), "ab…");
        assert_eq!(fit_chars("abc", 3), "abc");
        assert_eq!(fit_chars("abc", 0), "");
        assert_eq!(fit_chars("修复窗格焦点", 4), "修复窗…");
    }

    #[test]
    fn clip_bytes_respects_char_boundaries() {
        assert_eq!(clip_bytes("ab修复", 3), "ab");
        assert_eq!(clip_bytes("ab修复", 5), "ab修");
        assert_eq!(clip_bytes("abc", 10), "abc");
    }

    #[test]
    fn clean_line_returns_none_for_blank_text() {
        assert_eq!(clean_line("   \n ", 10), None);
        assert_eq!(clean_line(" hello\nworld ", 8).as_deref(), Some("hello w…"));
    }

    #[test]
    fn compact_counts() {
        assert_eq!(compact_count(950), "950");
        assert_eq!(compact_count(1_250), "1.2k");
        assert_eq!(compact_count(3_400_000), "3.4M");
    }

    #[test]
    fn strip_markdown_keeps_readable_text() {
        let text = "## Done\n\nUpdated **two** files:\n- `src/a.rs`\n- [docs](https://x.y/z)\n```rust\nfn main() {}\n```";
        assert_eq!(
            strip_markdown(text),
            "Done Updated two files: src/a.rs docs fn main() {}"
        );
        assert_eq!(
            summary_text("> Should the API client **retry** on 429?", 40).as_deref(),
            Some("Should the API client retry on 429?")
        );
        assert_eq!(
            strip_markdown("rename `focused_pane_id` to ~~old~~ new"),
            "rename focused_pane_id to old new"
        );
    }
}
