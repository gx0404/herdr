use super::*;
use crate::app::actions::safe_web_url;

/// Maximum number of hints offered at once: two letters over the full
/// alphabet. Scanning stops at the cap, so huge viewports stay cheap.
const LINK_HINT_CAP: usize = 26 * 26;
const LINK_HINT_LETTERS: &[u8; 26] = b"abcdefghijklmnopqrstuvwxyz";

/// One hintable URL region inside a pane viewport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ClientLinkHint {
    pub(super) label: String,
    pub(super) pane_id: String,
    /// Viewport row/column of the region start inside the pane's inner rect.
    pub(super) row: u16,
    pub(super) col: u16,
    pub(super) url: String,
}

/// Active link hints session: the scanned hints and the letters typed so far.
#[derive(Debug, Default)]
pub(super) struct ClientLinkHints {
    pub(super) hints: Vec<ClientLinkHint>,
    pub(super) typed: String,
}

fn hint_label(index: usize) -> String {
    let first = LINK_HINT_LETTERS[(index / 26) % 26];
    let second = LINK_HINT_LETTERS[index % 26];
    String::from_utf8(vec![first, second]).unwrap_or_default()
}

/// Balance-aware trailing trim mirroring the server-side URL resolver, so a
/// hint opens exactly what Ctrl+click would open.
fn trim_url_end(mut url: &str) -> &str {
    fn closer_balanced(head: &str, open: char, close: char) -> bool {
        let mut balance = 0i32;
        for ch in head.chars() {
            if ch == open {
                balance += 1;
            } else if ch == close {
                balance -= 1;
            }
        }
        balance > 0
    }
    loop {
        let Some(last) = url.chars().last() else {
            return url;
        };
        let head = &url[..url.len() - last.len_utf8()];
        let trim = match last {
            '"' | '\'' | '`' | '.' | ',' | ';' | ':' | '!' | '?' => true,
            ')' => !closer_balanced(head, '(', ')'),
            ']' => !closer_balanced(head, '[', ']'),
            '}' => !closer_balanced(head, '{', '}'),
            _ => false,
        };
        if !trim {
            return url;
        }
        url = head;
    }
}

/// Plain-text URL spans of one row: (start col, end col, url) in pane cells.
/// `cells` is the row slice of the pane's inner rect; byte offsets per cell
/// are tracked so wide glyphs keep column mapping exact.
fn row_text_url_spans(cells: &[crate::protocol::CellData]) -> Vec<(u16, u16, String)> {
    let mut text = String::new();
    let mut byte_cols = Vec::with_capacity(cells.len() + 1);
    for cell in cells {
        byte_cols.push(text.len());
        text.push_str(&cell.symbol);
    }
    let col_of_byte = |byte: usize| {
        byte_cols
            .partition_point(|offset| *offset <= byte)
            .saturating_sub(1)
    };
    let mut starts: Vec<usize> = text
        .match_indices("http://")
        .map(|(at, _)| at)
        .chain(text.match_indices("https://").map(|(at, _)| at))
        .collect();
    starts.sort_unstable();
    let mut spans = Vec::new();
    let mut covered_until = 0usize;
    for scheme_start in starts {
        if scheme_start < covered_until {
            continue;
        }
        let mut byte_end = text.len();
        for (offset, ch) in text[scheme_start..].char_indices() {
            if ch.is_whitespace() {
                byte_end = scheme_start + offset;
                break;
            }
        }
        let url = trim_url_end(&text[scheme_start..byte_end]);
        covered_until = byte_end;
        if url.len() <= "http://".len() || safe_web_url(url).is_none() {
            continue;
        }
        let start_col = col_of_byte(scheme_start);
        let end_col = col_of_byte(scheme_start + url.len().saturating_sub(1));
        spans.push((start_col as u16, end_col as u16, url.to_owned()));
    }
    spans
}

impl ClientShellState {
    /// Scan the visible viewport of every presented pane for hintable URLs:
    /// explicit OSC 8 hyperlink runs first, then plain-text http(s) URLs.
    /// Runs once when hints mode opens; capped and viewport-limited.
    fn collect_visible_link_hints(&self) -> Vec<ClientLinkHint> {
        let mut spans: Vec<(String, u16, u16, String)> = Vec::new();
        for hit in &self.hits.panes {
            let Some(surface) = self.visible_surface_for_pane(&hit.pane_id) else {
                continue;
            };
            let Some(pane) = surface
                .panes
                .iter()
                .find(|pane| pane.pane_id == hit.pane_id)
            else {
                continue;
            };
            if spans.len() >= LINK_HINT_CAP {
                break;
            }
            let rect = pane.inner_rect;
            for row in 0..rect.height {
                if spans.len() >= LINK_HINT_CAP {
                    break;
                }
                let start = (usize::from(rect.y) + usize::from(row))
                    * usize::from(surface.frame.width)
                    + usize::from(rect.x);
                let Some(cells) = surface
                    .frame
                    .cells
                    .get(start..start + usize::from(rect.width))
                else {
                    continue;
                };
                let mut explicit_cols = std::collections::HashSet::new();
                let mut col = 0usize;
                while col < cells.len() {
                    let Some(index) = cells[col].hyperlink else {
                        col += 1;
                        continue;
                    };
                    let run_start = col;
                    while col + 1 < cells.len() && cells[col + 1].hyperlink == Some(index) {
                        col += 1;
                    }
                    explicit_cols.insert(run_start as u16);
                    if let Some(uri) = surface.frame.hyperlinks.get(index as usize) {
                        if safe_web_url(uri).is_some() {
                            spans.push((pane.pane_id.clone(), row, run_start as u16, uri.clone()));
                        }
                    }
                    col += 1;
                }
                for (start_col, _, url) in row_text_url_spans(cells) {
                    if explicit_cols.contains(&start_col) {
                        continue;
                    }
                    spans.push((pane.pane_id.clone(), row, start_col, url));
                }
            }
        }
        spans.sort_by(|left, right| {
            (left.0.as_str(), left.1, left.2).cmp(&(right.0.as_str(), right.1, right.2))
        });
        spans
            .into_iter()
            .take(LINK_HINT_CAP)
            .enumerate()
            .map(|(index, (pane_id, row, col, url))| ClientLinkHint {
                label: hint_label(index),
                pane_id,
                row,
                col,
                url,
            })
            .collect()
    }

    pub(super) fn enter_link_hints(&mut self, outcome: &mut ClientShellInput) {
        let hints = self.collect_visible_link_hints();
        if hints.is_empty() {
            let message = crate::i18n::texts().overlays.link_hints_no_links.to_owned();
            self.record_notification(
                super::feedback::ClientToastLevel::Info,
                message.clone(),
                None,
                None,
            );
            if self.config.clipboard_toast_enabled {
                self.copy_feedback = Some(crate::app::state::CopyFeedback { message });
                self.copy_feedback_deadline =
                    Some(std::time::Instant::now() + std::time::Duration::from_secs(2));
            }
            outcome.repaint = true;
            return;
        }
        self.link_hints = Some(ClientLinkHints {
            hints,
            typed: String::new(),
        });
        outcome.repaint = true;
    }

    pub(super) fn route_link_hints_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) {
        outcome.repaint = true;
        if key.code == KeyCode::Esc {
            self.link_hints = None;
            return;
        }
        if key.code == KeyCode::Backspace {
            if let Some(hints) = self.link_hints.as_mut() {
                hints.typed.pop();
            }
            return;
        }
        let letter = match key.code {
            KeyCode::Char(ch) if key.modifiers.is_empty() && ch.is_ascii_alphabetic() => {
                ch.to_ascii_lowercase()
            }
            _ => {
                self.link_hints = None;
                return;
            }
        };
        let Some(hints) = self.link_hints.as_mut() else {
            return;
        };
        hints.typed.push(letter);
        let typed = hints.typed.clone();
        let exact = hints
            .hints
            .iter()
            .find(|hint| hint.label == typed)
            .map(|hint| hint.url.clone());
        let any_prefix = hints
            .hints
            .iter()
            .any(|hint| hint.label.starts_with(&typed));
        if let Some(url) = exact {
            self.link_hints = None;
            if safe_web_url(&url).is_some() {
                outcome.actions.push(ClientShellAction::OpenSafeWebUrl(url));
            }
        } else if !any_prefix {
            if let Some(hints) = self.link_hints.as_mut() {
                hints.typed.clear();
            }
        }
    }

    /// Draw the two-letter markers over the pane surface; markers whose label
    /// no longer matches the typed prefix dim down.
    pub(super) fn render_link_hints(
        &self,
        buffer: &mut Buffer,
        occlusion: &mut crate::kitty_graphics::surface::Occlusion,
    ) {
        let Some(hints) = self.link_hints.as_ref() else {
            return;
        };
        let palette = &self.config.palette;
        let active = Style::default()
            .fg(panel_contrast_fg(palette))
            .bg(palette.accent)
            .add_modifier(Modifier::BOLD);
        let inactive = Style::default().fg(palette.overlay0).bg(palette.surface0);
        for hint in &hints.hints {
            let Some(hit) = self
                .hits
                .panes
                .iter()
                .find(|hit| hit.pane_id == hint.pane_id)
            else {
                continue;
            };
            let x = hit.inner_rect.x.saturating_add(hint.col);
            let y = hit.inner_rect.y.saturating_add(hint.row);
            if x >= buffer.area.right() || y >= buffer.area.bottom() {
                continue;
            }
            let style = if hint.label.starts_with(&hints.typed) {
                active
            } else {
                inactive
            };
            let label_area = Rect::new(x, y, hint.label.len() as u16, 1).intersection(buffer.area);
            occlusion.cover(label_area);
            for (offset, ch) in hint.label.chars().enumerate() {
                let x = x.saturating_add(offset as u16);
                if x >= buffer.area.right() {
                    break;
                }
                let cell = &mut buffer[(x, y)];
                cell.set_symbol(&ch.to_string());
                cell.set_style(style);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(symbol: &str) -> crate::protocol::CellData {
        crate::protocol::CellData {
            symbol: symbol.into(),
            fg: 0,
            bg: 0,
            modifier: 0,
            skip: false,
            hyperlink: None,
        }
    }

    fn row(text: &str) -> Vec<crate::protocol::CellData> {
        text.chars().map(|ch| cell(&ch.to_string())).collect()
    }

    #[test]
    fn hint_labels_cover_two_letter_space() {
        assert_eq!(hint_label(0), "aa");
        assert_eq!(hint_label(25), "az");
        assert_eq!(hint_label(26), "ba");
        assert_eq!(hint_label(26 * 26 - 1), "zz");
    }

    #[test]
    fn row_spans_find_http_and_trim_trailing_punctuation() {
        let spans = row_text_url_spans(&row("see https://example.com/a(b). end"));
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].2, "https://example.com/a(b)");
        assert_eq!(spans[0].0, 4);
        assert_eq!(spans[0].1, 4 + "https://example.com/a(b)".len() as u16 - 1);
    }

    #[test]
    fn row_spans_skip_bare_scheme_and_non_http() {
        assert!(row_text_url_spans(&row("http:// done")).is_empty());
        assert!(row_text_url_spans(&row("ftp://example.com")).is_empty());
    }

    #[test]
    fn trim_url_end_respects_balanced_closers() {
        assert_eq!(trim_url_end("https://a.com/x),"), "https://a.com/x");
        assert_eq!(trim_url_end("https://a.com/x(y)"), "https://a.com/x(y)");
        assert_eq!(trim_url_end("https://a.com/x."), "https://a.com/x");
    }
}
