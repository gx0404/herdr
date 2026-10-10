//! harness 适配器共用的文本工具：压空白、按字符 / 字节截断、单行清洗、去 markdown、
//! 输出行清洗、计数缩写与字节偏移游标。各适配器原先各写一份（`collapse` /
//! `clip_chars` / `clip_bytes` / `clean_line` / `parse_cursor` …），语义在这里统一。

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

/// 千位以上的计数缩写，小数位截断：`950` / `1.2k`（1 290）/ `3.4M`。
pub fn compact_count(value: u64) -> String {
    if value < 1_000 {
        value.to_string()
    } else if value < 1_000_000 {
        format!("{}.{}k", value / 1_000, (value % 1_000) / 100)
    } else {
        format!("{}.{}M", value / 1_000_000, (value % 1_000_000) / 100_000)
    }
}

/// 千位以上的计数缩写，小数位四舍五入：`950` / `1.3k`（1 290）/ `3.4M`。
pub fn compact_count_rounded(value: u64) -> String {
    if value < 1_000 {
        value.to_string()
    } else if value < 1_000_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    }
}

/// 第一行非空文本（去掉首尾空白）。
pub fn first_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

/// 朴素标识符：非空、不超过 `max_len` 字节、只含 ASCII 字母数字与 `-` `_`。会话 /
/// 线程 / agent id 要拼进路径或 SQL 字面量前都先过这道闸（杜绝路径穿越与引号）。
pub fn is_plain_identifier(text: &str, max_len: usize) -> bool {
    !text.is_empty()
        && text.len() <= max_len
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// 十进制字节 / 行偏移游标（首尾空白不计）；不是非负整数时为 `None`，由调用方报
/// `Malformed`。
pub fn parse_offset(cursor: &str) -> Option<u64> {
    cursor.trim().parse().ok()
}

/// 内容片段只保留换行与制表符两种控制字符。
pub fn keep_line_breaks(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
        .collect()
}

/// 工具输出的清洗：去掉 ANSI 转义序列与控制字符（制表符留给客户端展开；
/// `keep_newlines` 时保留换行并去掉行尾空白），取前 `max_chars` 个字符，超出以省略号
/// 结尾。逐字符处理，读到上限就停。
pub fn clean_output(text: &str, max_chars: usize, keep_newlines: bool) -> String {
    let mut out = String::new();
    let mut kept = 0usize;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            // CSI（ESC [ … 终止字节 0x40–0x7e）整段丢；其余 ESC 连同下一个字符丢。
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
            } else {
                chars.next();
            }
            continue;
        }
        let keep = ch == '\t' || (keep_newlines && ch == '\n');
        if ch.is_control() && !keep {
            continue;
        }
        if kept == max_chars {
            out.push('…');
            break;
        }
        if ch == '\n' {
            out.truncate(out.trim_end_matches([' ', '\t']).len());
        }
        out.push(ch);
        kept += 1;
    }
    out.truncate(out.trim_end().len());
    out
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
        assert_eq!(compact_count(1_290), "1.2k", "截断");
        assert_eq!(compact_count(3_400_000), "3.4M");
        assert_eq!(compact_count_rounded(950), "950");
        assert_eq!(compact_count_rounded(1_290), "1.3k", "四舍五入");
        assert_eq!(compact_count_rounded(3_460_000), "3.5M");
    }

    #[test]
    fn plain_identifiers_reject_paths_quotes_and_overlong_text() {
        assert!(is_plain_identifier("ses_01-AB", 16));
        for bad in ["", "../x", "a/b", "a'b", "a b", "会话"] {
            assert!(!is_plain_identifier(bad, 16), "{bad:?}");
        }
        assert!(!is_plain_identifier("abcdef", 5));
    }

    #[test]
    fn offsets_and_first_lines() {
        assert_eq!(parse_offset("42"), Some(42));
        assert_eq!(parse_offset(" 7\n"), Some(7));
        assert_eq!(parse_offset("-1"), None);
        assert_eq!(parse_offset("t:3"), None);
        assert_eq!(
            first_line("\n  \n  second line \nthird"),
            Some("second line")
        );
        assert_eq!(first_line(" \n\t"), None);
    }

    #[test]
    fn output_cleaning_drops_escapes_and_controls() {
        assert_eq!(
            clean_output("\u{1b}[31mred\u{1b}[0m\u{7}\tok  ", 50, false),
            "red\tok"
        );
        assert_eq!(clean_output("ab\ncd", 50, false), "abcd");
        assert_eq!(clean_output("ab  \ncd\n", 50, true), "ab\ncd");
        assert_eq!(clean_output("abcdef", 3, false), "abc…");
        assert_eq!(keep_line_breaks("a\u{7}b\nc\td\r"), "ab\nc\td");
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
