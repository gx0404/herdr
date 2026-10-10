//! harness 适配器读 JSON 记录的小工具：字段缺失、类型不对一律当没有，不 panic。

use serde_json::Value;

/// 字符串字段，去掉首尾空白；空串当没有。
pub fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    trimmed(value.get(key))
}

/// 字符串值，去掉首尾空白；空串、非字符串当没有。
pub fn trimmed(value: Option<&Value>) -> Option<&str> {
    value?
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

/// 非负整数（毫秒时间戳、计数、偏移）：接受非负整数与非负有限浮点（截断）。
pub fn non_negative_integer(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        value
            .as_f64()
            .filter(|number| number.is_finite() && *number >= 0.0)
            .map(|number| number as u64)
    })
}

/// 消息内容里的正文：字符串原样；块数组取 `type` 属于 `kinds` 的块的 `text`，按换行
/// 连起来。没有正文时为 `None`。
pub fn text_content(content: &Value, kinds: &[&str]) -> Option<String> {
    let text = match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| {
                block
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| kinds.contains(&kind))
            })
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    (!text.trim().is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn string_fields_are_trimmed_and_blank_means_absent() {
        let value = json!({"a": "  x ", "b": "   ", "c": 3});
        assert_eq!(str_field(&value, "a"), Some("x"));
        assert_eq!(str_field(&value, "b"), None);
        assert_eq!(str_field(&value, "c"), None);
        assert_eq!(str_field(&value, "missing"), None);
        assert_eq!(trimmed(None), None);
    }

    #[test]
    fn non_negative_integers_accept_whole_and_fractional_numbers() {
        assert_eq!(non_negative_integer(&json!(12)), Some(12));
        assert_eq!(non_negative_integer(&json!(12.9)), Some(12));
        assert_eq!(non_negative_integer(&json!(-1)), None);
        assert_eq!(non_negative_integer(&json!("12")), None);
        assert_eq!(non_negative_integer(&json!(null)), None);
    }

    #[test]
    fn text_content_reads_strings_and_selected_blocks() {
        assert_eq!(
            text_content(&json!("plain"), &["text"]).as_deref(),
            Some("plain")
        );
        let blocks = json!([
            {"type": "thinking", "thinking": "hidden"},
            {"type": "text", "text": "first"},
            {"type": "tool_use", "name": "Bash"},
            {"type": "output_text", "text": "second"}
        ]);
        assert_eq!(
            text_content(&blocks, &["text", "output_text"]).as_deref(),
            Some("first\nsecond")
        );
        assert_eq!(text_content(&blocks, &["input_text"]), None);
        assert_eq!(text_content(&json!("  "), &["text"]), None);
        assert_eq!(text_content(&json!({"text": "x"}), &["text"]), None);
    }
}
