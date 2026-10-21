use super::super::*;
use crossterm::event::{KeyCode, MouseButton, MouseEvent, MouseEventKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::client::shell) enum Action {
    OpenFolder,
    NewWorkspace,
    Help,
}

pub(in crate::client::shell) fn paint(
    buffer: &mut Buffer,
    area: Rect,
    config: &ClientShellConfig,
    hover: Option<&super::super::feedback::ChromeHover>,
) -> Vec<(Rect, Action)> {
    let p = &config.palette;
    let t = &crate::i18n::texts().welcome;
    if area.height < 7 || area.width < 20 {
        return Vec::new();
    }
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            buffer[(x, y)].set_symbol(" ").set_style(
                crate::ui::design::body(p)
                    .bg(p.panel_bg)
                    .remove_modifier(Modifier::all()),
            );
        }
    }
    let rows = [
        (Action::OpenFolder, t.open_folder, Some("o".to_owned())),
        (
            Action::NewWorkspace,
            t.new_workspace,
            super::key_sequence(&config.keybinds, &config.keybinds.keybinds.new_workspace),
        ),
        (
            Action::Help,
            t.help,
            super::key_sequence(&config.keybinds, &config.keybinds.keybinds.help),
        ),
    ];
    let width = area.width.saturating_sub(4).min(52);
    let stacked = rows.iter().any(|(_, label, key)| {
        crate::ui::display_width_u16(label)
            .saturating_add(3)
            .saturating_add(key.as_deref().map_or(0, |key| {
                crate::ui::kit::key_cap::cap_width(key).saturating_add(1)
            }))
            > width
    });
    let header_rows = if area.height >= 9 { 3 } else { 1 };
    let height = header_rows + 6;
    let body = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    buffer.set_stringn(
        body.x + (width - 5) / 2,
        body.y,
        "herdr",
        5,
        crate::ui::design::title(p),
    );
    if header_rows > 1 {
        let subtitle = super::settings::truncate_with_ellipsis(t.subtitle, width);
        let subtitle_width = crate::ui::display_width_u16(&subtitle);
        buffer.set_stringn(
            body.x + (width - subtitle_width) / 2,
            body.y + 1,
            subtitle,
            usize::from(width),
            crate::ui::design::muted(p),
        );
    }
    let mut hits = Vec::new();
    for (index, (action, label, key)) in rows.into_iter().enumerate() {
        let y = body.y + header_rows + index as u16 * 2;
        let rect = Rect::new(body.x, y, width, 1);
        let hovered = hover == Some(&super::super::feedback::ChromeHover::WelcomeAction(action));
        super::settings::paint_options(
            buffer,
            rect,
            &super::settings::OptionList {
                rows: &[super::settings::OptionRow {
                    label: label.into(),
                    detail: "".into(),
                    key: key.as_deref().filter(|_| !stacked).map(Into::into),
                    current: false,
                }],
                start: 0,
                selected: 0,
                focused: action == Action::OpenFolder,
                hovered: hovered.then_some(0),
                gap: false,
            },
            p,
        );
        if let Some(key) = key.as_deref().filter(|_| stacked) {
            let label_style = buffer[(rect.x + 3, rect.y)].style();
            buffer.set_stringn(
                rect.x + 3,
                rect.y,
                label,
                usize::from(width - 3),
                label_style,
            );
            let bg = buffer[(rect.x, rect.y)].bg;
            let key_row = Rect::new(rect.x, y + 1, width, 1);
            buffer.set_style(key_row, Style::default().bg(bg));
            let key_width = crate::ui::kit::key_cap::cap_width(key);
            let inset = if key_width <= width.saturating_sub(3) {
                3
            } else {
                0
            };
            crate::ui::kit::key_cap::paint_cap(
                buffer,
                rect.x + inset,
                y + 1,
                rect.right(),
                key,
                super::bar_key_cap(p, bg),
            );
            hits.push((Rect::new(rect.x, rect.y, width, 2), action));
        } else {
            hits.push((rect, action));
        }
    }
    hits
}

impl ClientShellState {
    pub(in crate::client::shell) fn welcome_active(&self) -> bool {
        self.overlay.is_none()
            && self.observability.page.is_none()
            && self.observability.process_dialog.is_none()
            && (!self.workbench.enabled
                || matches!(
                    self.workbench.dock.focused,
                    super::super::dock::PanelId::Terminal(_)
                ))
            && self
                .snapshot
                .as_deref()
                .is_some_and(|snapshot| snapshot.workspaces.is_empty())
            && self.endpoint_status(&self.active_endpoint_id) == Some(ClientEndpointStatus::Online)
    }

    fn welcome_action(&mut self, action: Action, outcome: &mut ClientShellInput) {
        match action {
            Action::OpenFolder => {
                self.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
                    title: crate::i18n::texts().welcome.path_hint,
                    input: TextEditor::default(),
                    target: ClientRenameTarget::OpenDirectory,
                }))
            }
            Action::NewWorkspace | Action::Help => self.record_binding(
                crate::input::KeybindMatch::Action(if action == Action::NewWorkspace {
                    crate::input::KeybindAction::NewWorkspace
                } else {
                    crate::input::KeybindAction::Help
                }),
                outcome,
            ),
        }
        outcome.repaint = true;
    }

    pub(in crate::client::shell) fn welcome_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if !self.welcome_active() || !key.modifiers.is_empty() {
            return false;
        }
        let action = match key.code {
            KeyCode::Char('o') | KeyCode::Enter => Action::OpenFolder,
            KeyCode::Char('n') => Action::NewWorkspace,
            KeyCode::Char('?') => Action::Help,
            _ => return false,
        };
        self.welcome_action(action, outcome);
        true
    }

    pub(in crate::client::shell) fn welcome_mouse(
        &mut self,
        mouse: MouseEvent,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if !self.welcome_active() || !self.config.mouse_capture {
            return false;
        }
        let point = (mouse.column, mouse.row);
        if mouse.kind == MouseEventKind::Moved {
            self.update_chrome_hover(point, outcome);
            return false;
        }
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return false;
        }
        let action = self
            .hits
            .welcome_actions
            .iter()
            .find(|(rect, _)| contains(*rect, point))
            .map(|(_, action)| *action);
        if let Some(action) = action {
            self.welcome_action(action, outcome);
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn welcome_actions_stay_inside_the_area_at_all_gallery_sizes() {
        let _lang = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
        let config = ClientShellConfig::from_config(&Config::default());
        for (cols, rows) in [(160, 45), (80, 24), (60, 20)] {
            let area = Rect::new(0, 1, cols, rows - 2);
            let mut buffer = Buffer::empty(Rect::new(0, 0, cols, rows));
            let hits = paint(&mut buffer, area, &config, None);
            assert_eq!(hits.len(), 3);
            for (index, (rect, _)) in hits.iter().enumerate() {
                assert_eq!(rect.intersection(area), *rect);
                assert!(hits[index + 1..]
                    .iter()
                    .all(|(other, _)| !rect.intersects(*other)));
            }
        }
    }

    #[test]
    fn narrow_content_stacks_complete_action_names_and_live_key_hints() {
        for lang in [crate::i18n::Lang::ZhCn, crate::i18n::Lang::En] {
            let _lang = crate::i18n::lang_guard(lang);
            let config = ClientShellConfig::from_config(&Config::default());
            let t = &crate::i18n::texts().welcome;
            let expected = [
                (t.open_folder, "o".to_owned()),
                (
                    t.new_workspace,
                    super::super::key_sequence(
                        &config.keybinds,
                        &config.keybinds.keybinds.new_workspace,
                    )
                    .expect("new workspace binding"),
                ),
                (
                    t.help,
                    super::super::key_sequence(&config.keybinds, &config.keybinds.keybinds.help)
                        .expect("help binding"),
                ),
            ];
            for area in [
                Rect::new(26, 1, 33, 18),
                Rect::new(36, 1, 23, 18),
                Rect::new(0, 1, 20, 7),
            ] {
                let mut buffer = Buffer::empty(Rect::new(0, 0, 60, 20));
                let hits = paint(&mut buffer, area, &config, None);
                assert_eq!(hits.len(), expected.len());
                for ((rect, _), (label, key)) in hits.iter().zip(&expected) {
                    assert_eq!(rect.height, 2, "{lang:?} {area:?}");
                    assert_eq!(rect.intersection(area), *rect);
                    let pixels = &buffer;
                    let visible = (rect.y..rect.bottom())
                        .flat_map(|y| {
                            (rect.x..rect.right()).map(move |x| pixels[(x, y)].symbol().to_owned())
                        })
                        .collect::<String>()
                        .replace(' ', "");
                    assert!(
                        visible.contains(&label.replace(' ', "")),
                        "{lang:?} {area:?}: {label:?} / {visible:?}"
                    );
                    assert!(
                        visible.contains(&key.replace(' ', "")),
                        "{lang:?} {area:?}: {key:?} / {visible:?}"
                    );
                    assert!(!visible.contains('…'));
                }
            }
        }
    }
}
