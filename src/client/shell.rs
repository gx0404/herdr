use std::collections::{HashMap, HashSet, VecDeque};

mod action_table;
mod actions;
mod agent_activity_overlay;
mod agent_sidebar;
mod agent_tree;
mod aggregate_navigation;
mod workspace_navigation;
use workspace_navigation::WorkspaceNavigationTarget;
mod broadcast;
mod command_palette;
mod compose_canvas;
mod composition;
mod config;
mod context_menu;
mod copy_mode;
mod dock;
mod endpoint_agent_state;
mod endpoint_agents;
mod endpoint_navigation;
mod endpoint_notices;
mod endpoint_sidebar;
mod endpoints;
pub(in crate::client::shell) mod workbench;
pub(super) use endpoints::*;
mod feedback;
mod floating_pages;
mod frozen_selection;
mod global_menu;
mod graphics;
mod input;
mod input_source;
mod link_hints;
mod link_hover;
mod machine_auth_overlay;
mod machine_files_overlay;
mod machines_overlay;
mod mobile;
mod mouse;
mod notification_policy;
mod notifications;
mod observability;
mod overlay_input;
mod overlay_view;
mod page;
mod preferences;
mod render;
mod scenes_overlay;
mod scroll;
mod settings;
mod snippets_overlay;
mod state;
mod surface_patch;
mod text_editor;
mod which_key;
mod word_selection;
mod worktrees;
use text_editor::TextEditor;
use word_selection::ClientWordSelection;

pub(in crate::client::shell) use render::sidebar;
pub(crate) use state::*;
#[cfg(test)]
pub(super) use surface_patch::apply_composed_surface_patch;
pub(super) use surface_patch::{ClientComposedSurfacePatch, ClientPaneSurfacePatchOutcome};

use crossterm::event::KeyCode;
#[cfg(test)]
use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthStr;

use super::endpoint::{ClientEndpointId, ClientEndpointStatus, SavedSshEndpoint};
use crate::app::state::Palette;
use crate::config::{
    Config, LiveKeybindConfig, SidebarCollapsedModeConfig, SpacesSidebarConfig,
    TabBarPositionConfig,
};
use crate::protocol::{
    ClientMessage, ClientMousePosition, ClientPaneInputEvent, ClientShellSnapshot, ClientShellTab,
    ClientShellWorkspace, ClientSurfaceSize, FrameData, PaneSurfaceFrame, SemanticNotification,
    SemanticNotificationKind, SemanticNotificationSound,
};
#[cfg(test)]
use crate::raw_input::RawInputEvent;
use crate::ui::panel_contrast_fg;

fn target_event_message(target: ClientInputTarget, event: ClientPaneInputEvent) -> ClientMessage {
    match target {
        ClientInputTarget::Pane(pane_id) => ClientMessage::ClientShellPaneInput {
            pane_id,
            events: vec![event],
        },
        ClientInputTarget::Popup(terminal_id) => ClientMessage::ClientShellPopupInput {
            terminal_id,
            events: vec![event],
        },
    }
}

fn push_target_event(
    target: ClientInputTarget,
    event: ClientPaneInputEvent,
    outcome: &mut ClientShellInput,
) {
    match target {
        ClientInputTarget::Pane(pane_id) => {
            if let Some(ClientMessage::ClientShellPaneInput {
                pane_id: pending_pane,
                events,
            }) = outcome.requests.last_mut()
            {
                if *pending_pane == pane_id {
                    events.push(event);
                    return;
                }
            }
            outcome.requests.push(target_event_message(
                ClientInputTarget::Pane(pane_id),
                event,
            ));
        }
        ClientInputTarget::Popup(terminal_id) => {
            if let Some(ClientMessage::ClientShellPopupInput {
                terminal_id: pending_terminal,
                events,
            }) = outcome.requests.last_mut()
            {
                if *pending_terminal == terminal_id {
                    events.push(event);
                    return;
                }
            }
            outcome.requests.push(target_event_message(
                ClientInputTarget::Popup(terminal_id),
                event,
            ));
        }
    }
}

fn contains(rect: Rect, point: (u16, u16)) -> bool {
    rect.width > 0
        && rect.height > 0
        && point.0 >= rect.x
        && point.0 < rect.right()
        && point.1 >= rect.y
        && point.1 < rect.bottom()
}

fn pane_surface_topology_signature(surface: &PaneSurfaceFrame) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    fn write(hash: &mut u64, bytes: &[u8]) {
        for byte in bytes {
            *hash ^= u64::from(*byte);
            *hash = hash.wrapping_mul(PRIME);
        }
        *hash ^= 0xff;
        *hash = hash.wrapping_mul(PRIME);
    }

    let mut pane_ids = surface
        .panes
        .iter()
        .map(|pane| pane.pane_id.as_bytes())
        .collect::<Vec<_>>();
    pane_ids.sort_unstable();
    let mut hash = OFFSET;
    for pane_id in pane_ids {
        write(&mut hash, pane_id);
    }
    let mut splits = surface.splits.iter().collect::<Vec<_>>();
    splits.sort_by(|left, right| left.path.cmp(&right.path));
    for split in splits {
        write(
            &mut hash,
            &[match split.direction {
                crate::protocol::PaneSurfaceSplitDirection::Horizontal => 0,
                crate::protocol::PaneSurfaceSplitDirection::Vertical => 1,
            }],
        );
        write(
            &mut hash,
            &split
                .path
                .iter()
                .map(|right| u8::from(*right))
                .collect::<Vec<_>>(),
        );
    }
    hash
}

fn status_icon(
    status: crate::api::schema::AgentStatus,
    style: crate::config::StatusIndicatorStyle,
) -> &'static str {
    use crate::api::schema::AgentStatus;
    use crate::config::StatusIndicatorStyle;
    match (style, status) {
        (
            StatusIndicatorStyle::Dots,
            AgentStatus::Working | AgentStatus::Blocked | AgentStatus::Done,
        ) => "●",
        (StatusIndicatorStyle::Dots, AgentStatus::Idle) => "○",
        (StatusIndicatorStyle::Dots, AgentStatus::Unknown) => "·",
        (StatusIndicatorStyle::Symbols, AgentStatus::Blocked) => "×",
        (StatusIndicatorStyle::Symbols, AgentStatus::Working) => "◐",
        (StatusIndicatorStyle::Symbols, AgentStatus::Done) => "✓",
        (StatusIndicatorStyle::Symbols, AgentStatus::Idle) => "○",
        (StatusIndicatorStyle::Symbols, AgentStatus::Unknown) => "·",
    }
}

fn status_dot(status: crate::api::schema::AgentStatus) -> &'static str {
    status_icon(status, crate::config::StatusIndicatorStyle::Dots)
}

fn status_priority(status: crate::api::schema::AgentStatus) -> u8 {
    use crate::api::schema::AgentStatus;
    match status {
        AgentStatus::Blocked => 4,
        AgentStatus::Done => 3,
        AgentStatus::Working => 2,
        AgentStatus::Idle => 1,
        AgentStatus::Unknown => 0,
    }
}

fn status_text(status: crate::api::schema::AgentStatus) -> &'static str {
    use crate::api::schema::AgentStatus;
    match status {
        AgentStatus::Working => "working",
        AgentStatus::Blocked => "blocked",
        AgentStatus::Done => "done",
        AgentStatus::Idle => "idle",
        AgentStatus::Unknown => "unknown",
    }
}

fn status_color(
    status: crate::api::schema::AgentStatus,
    palette: &Palette,
) -> ratatui::style::Color {
    use crate::api::schema::AgentStatus;
    match status {
        AgentStatus::Working => palette.yellow,
        AgentStatus::Blocked => palette.red,
        AgentStatus::Done => palette.teal,
        AgentStatus::Idle => palette.green,
        AgentStatus::Unknown => palette.overlay0,
    }
}

/// 浮层列表行的底色，三态优先级：键盘选中（accent 反色）> 指针悬浮
/// （`components.hover_bg` 弱色）> 常态。hover 与 selected 分离之后两者必须
/// 视觉可辨，否则「鼠标路过」看起来就是「键盘选中」，回车激活的却是另一项
/// （MENU-01）。纯函数、无分配，供行循环按行调用。
fn list_row_bg(
    palette: &Palette,
    components: &crate::app::state::ComponentStyles,
    selected: bool,
    hovered: bool,
) -> ratatui::style::Color {
    if selected {
        palette.accent
    } else if hovered {
        components.hover_bg
    } else {
        palette.panel_bg
    }
}

/// `list_row_bg` 对应的整行样式：选中行反色加粗，悬浮行只换底色。
fn list_row_style(
    palette: &Palette,
    components: &crate::app::state::ComponentStyles,
    selected: bool,
    hovered: bool,
) -> Style {
    let style = Style::default().bg(list_row_bg(palette, components, selected, hovered));
    if selected {
        style
            .fg(panel_contrast_fg(palette))
            .add_modifier(Modifier::BOLD)
    } else {
        style.fg(palette.text)
    }
}

#[cfg(test)]
mod tests;
