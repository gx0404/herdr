use std::collections::{HashMap, HashSet, VecDeque};

mod action_table;
mod actions;
mod agent_activity_overlay;
mod agent_sidebar;
mod agent_tree;
mod aggregate_navigation;
mod workspace_navigation;
use workspace_navigation::{PendingWorkspaceHighlight, WorkspaceNavigationTarget};
mod broadcast;
mod chrome;
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
mod form;
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

/// 只有生命周期时的统一状态桶（没有失败信息，按未失败处理）。所有状态字形、颜色与
/// 排序都经桶取，保证侧栏、标签、总览与通知用同一套语义。
fn agent_bucket(status: crate::api::schema::AgentStatus) -> crate::harness::AgentBucket {
    crate::harness::AgentBucket::derive(status, false)
}

/// 快照里一个 agent 的统一状态桶：生命周期加 server 下发的失败锁存
/// （`insight.attention` 为 failed）。
fn client_agent_bucket(agent: &crate::protocol::ClientShellAgent) -> crate::harness::AgentBucket {
    let failed =
        agent.insight.attention.as_ref().is_some_and(|attention| {
            attention.kind == crate::api::schema::AgentAttentionKind::Failed
        });
    crate::harness::AgentBucket::derive(agent.agent_status, failed)
}

/// 状态字形的取法：指示风格 + 全局转圈帧。`animate` 为假（`ui.animations` 或
/// `ui.spinner` 关闭）时运行中画静态字形。按值传递、不分配；侧栏行、标签与标题栏
/// 计数共用，所有转圈同相位。
#[derive(Clone, Copy, Debug)]
struct StatusGlyphs {
    style: crate::config::StatusIndicatorStyle,
    frame: usize,
    animate: bool,
}

impl StatusGlyphs {
    /// 不转圈的取法：运行中画静态字形。
    const fn fixed(style: crate::config::StatusIndicatorStyle) -> Self {
        Self {
            style,
            frame: 0,
            animate: false,
        }
    }

    fn bucket(self, bucket: crate::harness::AgentBucket) -> &'static str {
        crate::ui::design::status_glyph(bucket, self.style, self.frame, self.animate)
    }
}

/// 静态状态字形（运行中画静态字形；需要转圈的界面用 [`StatusGlyphs`]）。
fn status_icon(
    status: crate::api::schema::AgentStatus,
    style: crate::config::StatusIndicatorStyle,
) -> &'static str {
    crate::ui::design::status_glyph(agent_bucket(status), style, 0, false)
}

fn status_dot(status: crate::api::schema::AgentStatus) -> &'static str {
    status_icon(status, crate::config::StatusIndicatorStyle::Dots)
}

/// 汇总优先级：需要输入 > 失败 > 运行中 > 待查看 > 空闲 > 未知。
fn status_priority(status: crate::api::schema::AgentStatus) -> u8 {
    agent_bucket(status).rollup_rank()
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

/// 状态色：取 `ComponentStyles` 的状态 token（蓝 = 运行、琥珀 = 需要输入、绿 = 待查看、
/// 红 = 失败、灰 = 空闲），主题可在 `[theme.components]` 覆盖。
fn status_color(
    status: crate::api::schema::AgentStatus,
    palette: &Palette,
    components: &crate::app::state::ComponentStyles,
) -> ratatui::style::Color {
    crate::ui::design::status_color(agent_bucket(status), palette, components)
}

/// 浮层列表行的底色，三态优先级：键盘选中（中性选中底色）> 指针悬浮
/// （`components.hover_bg`）> 常态。Graphite 里选中与悬浮的底色几乎相同，选中行
/// 另在首列画 accent `▌`（`kit::paint_selection_mark`）并加粗，两者始终可辨——否则
/// 「鼠标路过」看起来就是「键盘选中」，回车激活的却是另一项（MENU-01）。纯函数、
/// 无分配，供行循环按行调用。
fn list_row_bg(
    palette: &Palette,
    components: &crate::app::state::ComponentStyles,
    selected: bool,
    hovered: bool,
) -> ratatui::style::Color {
    if selected {
        palette.selection_row_bg()
    } else if hovered {
        components.hover_bg
    } else {
        palette.panel_bg
    }
}

/// `list_row_bg` 对应的整行样式：选中行是设计语言的选中行文字
/// （`design::selected_row_text`：选中底色 + 读得清的正文 + 粗体），悬浮行只换底色。
fn list_row_style(
    palette: &Palette,
    components: &crate::app::state::ComponentStyles,
    selected: bool,
    hovered: bool,
) -> Style {
    if selected {
        return crate::ui::design::selected_row_text(palette);
    }
    Style::default()
        .bg(list_row_bg(palette, components, selected, hovered))
        .fg(palette.text)
}

/// 浮层列表选中行的首列标记：选中行逐行在首列画 accent `▌`，悬浮行不画——选中与
/// 悬浮因此始终可辨（MENU-01）。在行文字画完之后调用（行首的空格会盖掉首列）。
fn paint_list_selection(buffer: &mut Buffer, row: Rect, selected: bool, palette: &Palette) {
    if !selected {
        return;
    }
    for y in row.y..row.bottom() {
        crate::ui::kit::paint_selection_mark(buffer, row.x, y, palette);
    }
}

#[cfg(test)]
mod tests;
