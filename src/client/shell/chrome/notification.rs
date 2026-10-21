//! 语义通知的正文折行与状态桶判定；卡片的版式与绘制在 [`super::toast`]。

use super::super::*;
use crate::ui::display_width_u16;
use unicode_segmentation::UnicodeSegmentation;

pub(in crate::client::shell) fn body_lines(text: &str, width: u16) -> Vec<String> {
    if width == 0 || text.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![String::new()];
    let mut used = 0u16;
    for grapheme in text.graphemes(true) {
        let size = display_width_u16(grapheme);
        if grapheme == "\n" || used.saturating_add(size) > width {
            if lines.len() == 2 {
                if let Some(last) = lines.last_mut() {
                    *last = super::settings::truncate_with_ellipsis(&format!("{last}…"), width);
                }
                break;
            }
            lines.push(String::new());
            used = 0;
            if grapheme == "\n" {
                continue;
            }
        }
        if size <= width {
            if let Some(line) = lines.last_mut() {
                line.push_str(grapheme);
            }
            used += size;
        }
    }
    lines
}

pub(in crate::client::shell) fn bucket(
    event: &SemanticNotification,
    endpoints: &[ClientShellEndpoint],
    endpoint: &ClientEndpointId,
) -> crate::harness::AgentBucket {
    use crate::harness::AgentBucket;
    match event.kind {
        SemanticNotificationKind::NeedsAttention => AgentBucket::NeedsInput,
        SemanticNotificationKind::Finished => AgentBucket::Review,
        SemanticNotificationKind::Custom
            if event.agent.is_some()
                && (endpoints
                    .iter()
                    .find(|e| &e.endpoint_id == endpoint)
                    .and_then(|e| e.snapshot.as_deref())
                    .and_then(|s| {
                        s.agents
                            .iter()
                            .find(|a| Some(a.pane_id.as_str()) == event.pane_id.as_deref())
                    })
                    .is_some_and(|a| client_agent_bucket(a) == AgentBucket::Failed)
                    || event.title.contains(crate::i18n::en::TEXTS.notify.failed)
                    || event
                        .title
                        .contains(crate::i18n::zh_cn::TEXTS.notify.failed)) =>
        {
            AgentBucket::Failed
        }
        _ => AgentBucket::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn message_wrap_is_bounded_and_cjk_safe() {
        assert_eq!(
            body_lines("等待确认\n继续处理", 8),
            vec!["等待确认", "继续处理"]
        );
        let lines = body_lines("许可请求包含中文以及a-long-command-that-does-not-fit", 12);
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|line| display_width_u16(line) <= 12));
        assert!(lines[1].ends_with('…'));
        assert!(body_lines("", 12).is_empty());
    }
}
