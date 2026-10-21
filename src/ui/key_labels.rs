//! 按键文字的显示口径：键帽、页脚、帮助与 which-key 上的按键一律写成小写词——
//! `ctrl+b g`、`ctrl+enter`、`enter`、`esc`、`tab`、`space`。同时按用 `+`，顺序按键用
//! 空格，备选用 ` / `；单个字符保留大小写（`r` 与 `R` 是两个键）。
//!
//! 只改显示，不改绑定：配置解析与 round-trip 仍用 `config::keybinds` 的序列化标签，
//! 它本身就是这套小写口径（`format_key_combo`）。已是规范写法时原样借用、不分配，
//! 可以在每帧的页脚绘制里调用。

use std::borrow::Cow;

/// 配置里前缀绑定的写法（`prefix+g`），显示时展开成实际的前缀键。
const PREFIX_MARKER: &str = "prefix+";

/// 一个按键标签的规范显示写法（见模块说明）：`^c` 这类脱字符写法展开为 `ctrl+c`，
/// `↵` / `⏎` / `Enter` 写成 `enter`，`Esc` / `Escape` 写成 `esc`，修饰键与具名键一律
/// 小写；单字符键、方向符号（`↑↓`）与区间（`1-9`）原样保留。
pub(crate) fn display_key(label: &str) -> Cow<'_, str> {
    if !needs_rewrite(label) {
        return Cow::Borrowed(label);
    }
    let mut out = String::with_capacity(label.len() + 4);
    for (index, alternative) in label.trim().split(" / ").enumerate() {
        if index > 0 {
            out.push_str(" / ");
        }
        for (step_index, step) in alternative.split_whitespace().enumerate() {
            if step_index > 0 {
                out.push(' ');
            }
            push_step(&mut out, step);
        }
    }
    Cow::Owned(out)
}

/// 把配置里的 `prefix+x` 展开成 `<前缀键> x`（备选各自展开），其余写法同
/// [`display_key`]。`prefix_label` 是实际的前缀键（`LiveKeybindConfig::primary_prefix_label`）。
pub(crate) fn with_prefix(label: &str, prefix_label: &str) -> String {
    let prefix = display_key(prefix_label);
    let mut out = String::with_capacity(label.len() + prefix.len() + 4);
    for (index, alternative) in label.trim().split(" / ").enumerate() {
        if index > 0 {
            out.push_str(" / ");
        }
        match alternative.trim().strip_prefix(PREFIX_MARKER) {
            Some(rest) => {
                out.push_str(&prefix);
                out.push(' ');
                out.push_str(&display_key(rest));
            }
            None => out.push_str(&display_key(alternative.trim())),
        }
    }
    out
}

/// 规范写法原样借用：只有存在要改写的片段时才分配。
fn needs_rewrite(label: &str) -> bool {
    if label.trim().len() != label.len() {
        return true;
    }
    for alternative in label.split(" / ") {
        if alternative.trim().len() != alternative.len() || alternative.contains("  ") {
            return true;
        }
        for step in alternative.split(' ') {
            if step.is_empty() {
                return true;
            }
            if caret_key(step).is_some() {
                return true;
            }
            let (modifiers, key) = split_combo(step);
            if modifiers
                .split('+')
                .filter(|part| !part.is_empty())
                .any(|part| canonical_modifier(part) != Some(part))
            {
                return true;
            }
            if rewrite_key(key).is_some() {
                return true;
            }
        }
    }
    false
}

/// 一个顺序按键（`ctrl+shift+tab`、`^w`、`g`）写进 `out`。
fn push_step(out: &mut String, step: &str) {
    if let Some(letter) = caret_key(step) {
        out.push_str("ctrl+");
        out.push(letter.to_ascii_lowercase());
        return;
    }
    let (modifiers, key) = split_combo(step);
    for part in modifiers.split('+').filter(|part| !part.is_empty()) {
        match canonical_modifier(part) {
            Some(canonical) => out.push_str(canonical),
            None => out.push_str(part),
        }
        out.push('+');
    }
    match rewrite_key(key) {
        Some(Cow::Borrowed(canonical)) => out.push_str(canonical),
        Some(Cow::Owned(lowered)) => out.push_str(&lowered),
        None => out.push_str(key),
    }
}

/// `^c` 形式的脱字符写法：返回其中的字母。
fn caret_key(step: &str) -> Option<char> {
    let mut chars = step.chars();
    match (chars.next(), chars.next(), chars.next()) {
        (Some('^'), Some(letter), None) if letter.is_ascii_alphabetic() => Some(letter),
        _ => None,
    }
}

/// 拆出修饰键部分与最后的按键：`ctrl+shift+tab` → (`ctrl+shift`, `tab`)。`+` 本身也是
/// 键：`+` → (``, `+`)，`ctrl++` → (`ctrl`, `+`)。
fn split_combo(step: &str) -> (&str, &str) {
    if step == "+" {
        return ("", step);
    }
    if let Some(modifiers) = step.strip_suffix("++") {
        return (modifiers, "+");
    }
    match step.rfind('+') {
        Some(index) if index + 1 < step.len() => (&step[..index], &step[index + 1..]),
        _ => ("", step),
    }
}

fn canonical_modifier(part: &str) -> Option<&'static str> {
    Some(match part.to_ascii_lowercase().as_str() {
        "ctrl" | "control" | "⌃" => "ctrl",
        "shift" | "⇧" => "shift",
        "alt" | "option" | "opt" | "⌥" => "alt",
        "meta" => "meta",
        "cmd" | "command" | "⌘" => "cmd",
        "super" | "win" => "super",
        "hyper" => "hyper",
        _ => return None,
    })
}

/// 需要改写的按键返回规范写法；已规范或不认识的（单字符、方向符号、区间）返回 `None`。
fn rewrite_key(key: &str) -> Option<Cow<'static, str>> {
    let canonical = match key {
        "↵" | "⏎" | "⌤" => "enter",
        "⎋" => "esc",
        "⇥" => "tab",
        "␣" | "空格" => "space",
        "⌫" => "backspace",
        "⌦" => "delete",
        _ => {
            if key.chars().count() <= 1 || !key.is_ascii() {
                return None;
            }
            match key.to_ascii_lowercase().as_str() {
                "return" => "enter",
                "escape" => "esc",
                "spc" => "space",
                "bksp" => "backspace",
                "del" => "delete",
                lowered
                    if lowered != key && lowered.chars().all(|ch| ch.is_ascii_alphanumeric()) =>
                {
                    return Some(Cow::Owned(lowered.to_owned()));
                }
                _ => return None,
            }
        }
    };
    (canonical != key).then_some(Cow::Borrowed(canonical))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_labels_are_borrowed_unchanged() {
        for label in [
            "ctrl+b g",
            "ctrl+enter",
            "enter",
            "esc",
            "tab",
            "space",
            "shift+tab",
            "r",
            "R",
            "?",
            "/",
            "+",
            "ctrl++",
            "↑↓",
            "←→",
            "1-9",
            "f5",
            "ctrl+b / ctrl+a",
            "pageup",
        ] {
            assert!(
                matches!(display_key(label), Cow::Borrowed(text) if text == label),
                "{label}"
            );
        }
    }

    #[test]
    fn variant_spellings_collapse_to_the_lowercase_words() {
        for (label, expected) in [
            ("Enter", "enter"),
            ("↵", "enter"),
            ("Return", "enter"),
            ("Esc", "esc"),
            ("Escape", "esc"),
            ("Tab", "tab"),
            ("Shift+Tab", "shift+tab"),
            ("Space", "space"),
            ("空格", "space"),
            ("Ctrl+K", "ctrl+K"),
            ("Ctrl+Enter", "ctrl+enter"),
            ("^c", "ctrl+c"),
            ("^W", "ctrl+w"),
            ("PgUp", "pgup"),
            ("F5", "f5"),
            ("  esc ", "esc"),
            ("Ctrl+B  G", "ctrl+B G"),
            ("Enter / Esc", "enter / esc"),
        ] {
            assert_eq!(display_key(label), expected, "{label}");
        }
    }

    #[test]
    fn prefix_bindings_expand_to_the_live_prefix() {
        assert_eq!(with_prefix("prefix+g", "ctrl+b"), "ctrl+b g");
        assert_eq!(
            with_prefix("prefix+g / prefix+G", "ctrl+b"),
            "ctrl+b g / ctrl+b G"
        );
        assert_eq!(with_prefix("ctrl+k", "ctrl+b"), "ctrl+k");
        assert_eq!(with_prefix("prefix+Enter", "Ctrl+A"), "ctrl+A enter");
        assert_eq!(with_prefix("prefix+/", "ctrl+b"), "ctrl+b /");
    }
}
