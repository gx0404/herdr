use ratatui::{layout::Rect, Frame};

use super::panes::{compute_pane_infos_for_tab, render_panes, resize_tab_panes};
use crate::app::AppState;
use crate::layout::{PaneId, PaneInfo, SplitBorder};
use crate::protocol::CursorState;
use crate::terminal::{TerminalRuntime, TerminalRuntimeRegistry};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TabSurfaceTarget {
    pub(crate) workspace_index: usize,
    pub(crate) tab_index: usize,
}

pub(crate) struct TabSurfaceLayout {
    pub(crate) target: Option<TabSurfaceTarget>,
    pub(crate) pane_infos: Vec<PaneInfo>,
    pub(crate) split_borders: Vec<SplitBorder>,
}

#[derive(Clone, Copy)]
pub(crate) struct TabSurfaceView<'a> {
    pub(crate) target: Option<TabSurfaceTarget>,
    pub(crate) pane_infos: &'a [PaneInfo],
    pub(crate) split_borders: &'a [SplitBorder],
}

pub(crate) fn compute_tab_surface(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    area: Rect,
    resize_panes: bool,
    cell_size: crate::kitty_graphics::HostCellSize,
) -> TabSurfaceLayout {
    let target = app.active.and_then(|workspace_index| {
        let workspace = app.workspaces.get(workspace_index)?;
        Some(TabSurfaceTarget {
            workspace_index,
            tab_index: workspace.active_tab_index(),
        })
    });
    compute_tab_surface_for(
        app,
        terminal_runtimes,
        target,
        area,
        resize_panes,
        cell_size,
    )
}

pub(crate) fn compute_tab_surface_for(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    target: Option<TabSurfaceTarget>,
    area: Rect,
    resize_panes: bool,
    cell_size: crate::kitty_graphics::HostCellSize,
) -> TabSurfaceLayout {
    let tab = target.and_then(|target| {
        app.workspaces
            .get(target.workspace_index)?
            .tabs
            .get(target.tab_index)
    });
    let split_borders = tab
        .map(|tab| {
            if tab.zoomed {
                Vec::new()
            } else {
                tab.layout.splits(area)
            }
        })
        .unwrap_or_default();
    let pane_infos = target.map_or_else(Vec::new, |target| {
        compute_pane_infos_for_tab(
            app,
            terminal_runtimes,
            target.workspace_index,
            target.tab_index,
            area,
            resize_panes,
            cell_size,
        )
    });

    TabSurfaceLayout {
        target,
        pane_infos,
        split_borders,
    }
}

pub(crate) fn resize_tab_surface(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    workspace_index: usize,
    tab_index: usize,
    area: Rect,
    cell_size: crate::kitty_graphics::HostCellSize,
) {
    let Some(tab) = app
        .workspaces
        .get(workspace_index)
        .and_then(|workspace| workspace.tabs.get(tab_index))
    else {
        return;
    };
    resize_tab_panes(
        app,
        terminal_runtimes,
        workspace_index,
        tab,
        area,
        cell_size,
    );
}

pub(crate) fn render_tab_surface(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    surface: TabSurfaceView<'_>,
    frame: &mut Frame,
) {
    render_panes(
        app,
        terminal_runtimes,
        frame,
        surface.target,
        surface.pane_infos,
        surface.split_borders,
    );
}

pub(crate) fn tab_surface_hyperlinks(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    surface: TabSurfaceView<'_>,
) -> Vec<((u16, u16), String, String)> {
    let Some(ws_idx) = surface.target.map(|target| target.workspace_index) else {
        return Vec::new();
    };
    if app.workspaces.get(ws_idx).is_none() {
        return Vec::new();
    }

    let mut links = Vec::new();
    for info in surface.pane_infos {
        if let Some(runtime) = app.runtime_for_pane_in_workspace(terminal_runtimes, ws_idx, info.id)
        {
            links.extend(runtime.visible_hyperlinks(info.inner_rect));
        }
    }
    links
}

pub(crate) fn tab_surface_cursor(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    surface: TabSurfaceView<'_>,
) -> Option<CursorState> {
    let ws_idx = surface.target?.workspace_index;
    let info = surface.pane_infos.iter().find(|info| info.is_focused)?;
    let runtime = app.runtime_for_pane_in_workspace(terminal_runtimes, ws_idx, info.id)?;
    pane_host_cursor(
        runtime,
        PaneHostCursorInputs {
            area: info.inner_rect,
            reveal: cjk_ime_reveal(app, ws_idx, info.id),
            scrolled_back: super::panes::pane_is_scrolled_back(runtime),
            reveal_shape: app.cjk_ime_cursor_shape,
        },
    )
}

/// 宿主光标解析的输入。完整渲染器 `tab_surface_cursor` 与 headless retained
/// 快路径 `server::headless::retained_surface::retained_cursor` 都由它构造并调用
/// `pane_host_cursor`，两条出口对同一 pane 输出同一宿主光标；attach 直渲
/// `server::render_stream::render_terminal_virtual` 没有 IME 揭示，但依赖同一
/// `runtime.cursor_state` 语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PaneHostCursorInputs {
    /// pane 内容区：光标坐标相对它偏移，落在区域外视为无光标。
    pub area: Rect,
    /// CJK IME 揭示：即使 pane 隐藏了光标也给宿主一个可见锚点。
    pub reveal: bool,
    /// 正在回看历史：不显示光标。
    pub scrolled_back: bool,
    /// 揭示时使用的光标形状。
    pub reveal_shape: u8,
}

/// 由 pane 运行时光标解析宿主光标的纯函数。DECSET 2026 批次进行中
/// `runtime.cursor_state` 沿用批次前快照的光标（pane 层看门狗，超时自动失效），
/// 这里不再整帧返回 None，宿主不会收到一次 `?25l` 闪断。
pub(crate) fn pane_host_cursor(
    runtime: &TerminalRuntime,
    inputs: PaneHostCursorInputs,
) -> Option<CursorState> {
    if let Some(cursor) = runtime.cursor_state(inputs.area, true) {
        let visible = if inputs.reveal {
            !inputs.scrolled_back
        } else {
            cursor.visible && !inputs.scrolled_back
        };
        Some(CursorState {
            x: cursor.x,
            y: cursor.y,
            visible,
            shape: if inputs.reveal && visible {
                inputs.reveal_shape
            } else {
                cursor.shape
            },
        })
    } else if inputs.reveal && !inputs.scrolled_back {
        Some(CursorState {
            x: inputs.area.x,
            y: inputs.area.y,
            visible: true,
            shape: inputs.reveal_shape,
        })
    } else {
        None
    }
}

/// 该 pane 是否启用 CJK IME 光标揭示：全局开关打开，且未配置 agent 过滤或
/// 检测到的 agent 在过滤列表内。
pub(crate) fn cjk_ime_reveal(app: &AppState, ws_idx: usize, pane_id: PaneId) -> bool {
    app.reveal_hidden_cursor_for_cjk_ime
        && (!app.cjk_ime_agent_filter_configured || {
            let detected = app
                .workspaces
                .get(ws_idx)
                .and_then(|ws| ws.terminal_id(pane_id))
                .and_then(|terminal_id| app.terminals.get(terminal_id))
                .and_then(|terminal| terminal.detected_agent);
            detected.is_some_and(|agent| app.cjk_ime_agents.contains(&agent))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::Workspace;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Direction;
    use ratatui::Terminal;

    #[tokio::test]
    async fn explicit_surface_layout_drives_render_cursor_and_hyperlinks() {
        let uri = "https://example.com/surface";
        let mut workspace = Workspace::test_new("shell-workspace");
        let left = workspace.tabs[0].root_pane;
        let right = workspace.test_split(Direction::Horizontal);
        workspace.insert_test_runtime(
            left,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(
                20,
                8,
                format!("\x1b]8;;{uri}\x1b\\LEFT\x1b]8;;\x1b\\").as_bytes(),
            ),
        );
        workspace.insert_test_runtime(
            right,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 8, b"RIGHT"),
        );

        let mut app = AppState::test_new();
        app.workspaces = vec![workspace];
        app.active = Some(0);
        app.selected = 0;

        let full_area = Rect::new(0, 0, 106, 20);
        let area = full_area;
        let surface = compute_tab_surface(
            &app,
            &TerminalRuntimeRegistry::new(),
            area,
            false,
            crate::kitty_graphics::HostCellSize::default(),
        );
        assert_eq!(surface.pane_infos.len(), 2);
        assert!(!surface.split_borders.is_empty());

        app.view.terminal_area = Rect::new(9, 8, 7, 6);
        app.view.pane_infos.clear();

        let surface_view = TabSurfaceView {
            target: surface.target,
            pane_infos: &surface.pane_infos,
            split_borders: &surface.split_borders,
        };
        let mut terminal =
            Terminal::new(TestBackend::new(full_area.width, full_area.height)).unwrap();
        terminal
            .draw(|frame| {
                render_tab_surface(&app, &TerminalRuntimeRegistry::new(), surface_view, frame)
            })
            .unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("LEFT"), "surface: {rendered:?}");
        assert!(rendered.contains("RIGHT"), "surface: {rendered:?}");
        assert!(!rendered.contains("shell-workspace"));

        let links = tab_surface_hyperlinks(&app, &TerminalRuntimeRegistry::new(), surface_view);
        assert!(links
            .iter()
            .any(|(_, symbol, link)| { symbol == "L" && link == uri }));
        assert!(tab_surface_cursor(&app, &TerminalRuntimeRegistry::new(), surface_view,).is_some());
    }

    /// DECSET 2026 批次进行中沿用上一帧光标（不再整帧 `?25l`），`2026l` 后恢复
    /// 为当前可见光标。
    #[tokio::test]
    async fn synchronized_output_keeps_the_previous_cursor_until_the_batch_ends() {
        let mut workspace = Workspace::test_new("sync-workspace");
        let pane = workspace.tabs[0].root_pane;
        workspace.insert_test_runtime(
            pane,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 8, b"READY"),
        );

        let mut app = AppState::test_new();
        app.workspaces = vec![workspace];
        app.active = Some(0);
        app.selected = 0;
        let registry = TerminalRuntimeRegistry::new();
        let runtime = app
            .runtime_for_pane_in_workspace(&registry, 0, pane)
            .expect("测试运行时");
        let area = Rect::new(0, 0, 20, 8);
        let surface = compute_tab_surface(
            &app,
            &TerminalRuntimeRegistry::new(),
            area,
            false,
            crate::kitty_graphics::HostCellSize::default(),
        );
        let view = TabSurfaceView {
            target: surface.target,
            pane_infos: &surface.pane_infos,
            split_borders: &surface.split_borders,
        };
        let before =
            tab_surface_cursor(&app, &TerminalRuntimeRegistry::new(), view).expect("批次前有光标");
        assert!(before.visible);

        runtime.test_process_pty_bytes(b"\x1b[?2026h\x1b[3;3H\x1b[?25l");
        assert!(runtime.synchronized_output_active());
        let during = tab_surface_cursor(&app, &TerminalRuntimeRegistry::new(), view);
        assert_eq!(
            during,
            Some(before.clone()),
            "同步输出期间沿用上一帧光标，而不是整帧隐藏"
        );

        // 置位超过 1 s 未复位：看门狗失效，经真实出口读到当前光标。
        runtime.test_backdate_synchronized_output(std::time::Duration::from_secs(1));
        assert!(!runtime.synchronized_output_active());
        let expired = tab_surface_cursor(&app, &TerminalRuntimeRegistry::new(), view)
            .expect("看门狗到期后有光标");
        assert_eq!((expired.x, expired.y), (2, 2), "到期后恢复为当前位置");
        assert!(
            expired.visible,
            "用户配置 reveal 恒真：隐藏光标也给宿主锚点"
        );

        runtime.test_process_pty_bytes(b"\x1b[?2026l\x1b[?25h");
        assert!(!runtime.synchronized_output_active());
        let after = tab_surface_cursor(&app, &TerminalRuntimeRegistry::new(), view)
            .expect("批次结束后有光标");
        assert!(after.visible);
        assert_eq!((after.x, after.y), (2, 2), "批次内移动的光标在复位后生效");
    }

    /// `pane_host_cursor` 是 `tab_surface_cursor` 与 `retained_cursor` 共用的纯函数：
    /// 表驱动覆盖 2026 批次内 / 看门狗到期 / 批次结束 / 无历史光标 / 揭示 /
    /// 回看历史六种输入。
    #[tokio::test]
    async fn pane_host_cursor_resolves_the_same_cursor_for_both_exits() {
        use std::time::Duration;
        let runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 8, b"READY");
        let area = Rect::new(10, 3, 20, 8);
        let plain = PaneHostCursorInputs {
            area,
            reveal: false,
            scrolled_back: false,
            reveal_shape: 6,
        };
        let reveal = PaneHostCursorInputs {
            reveal: true,
            ..plain
        };
        let cursor = |x: u16, y: u16, visible: bool, shape: u8| CursorState {
            x,
            y,
            visible,
            shape,
        };

        // 批次外：读当前光标并按区域偏移；揭示只改形状。
        assert_eq!(
            pane_host_cursor(&runtime, plain),
            Some(cursor(15, 3, true, 0))
        );
        assert_eq!(
            pane_host_cursor(&runtime, reveal),
            Some(cursor(15, 3, true, 6))
        );
        // 回看历史：不显示光标，形状保持 pane 自己的。
        assert_eq!(
            pane_host_cursor(
                &runtime,
                PaneHostCursorInputs {
                    scrolled_back: true,
                    ..reveal
                }
            ),
            Some(cursor(15, 3, false, 0))
        );

        // 2026 批次内（隐藏并移动）：沿用批次前光标。
        runtime.test_process_pty_bytes(b"\x1b[?2026h\x1b[?25l\x1b[3;3H");
        assert_eq!(
            pane_host_cursor(&runtime, plain),
            Some(cursor(15, 3, true, 0)),
            "批次内沿用批次前光标"
        );
        // 看门狗到期：读当前（隐藏）光标；揭示把它变成可见锚点。
        runtime.test_backdate_synchronized_output(Duration::from_secs(1));
        assert_eq!(
            pane_host_cursor(&runtime, plain),
            Some(cursor(12, 5, false, 0))
        );
        assert_eq!(
            pane_host_cursor(&runtime, reveal),
            Some(cursor(12, 5, true, 6))
        );
        // 批次结束：当前可见光标。
        runtime.test_process_pty_bytes(b"\x1b[?2026l\x1b[?25h");
        assert_eq!(
            pane_host_cursor(&runtime, plain),
            Some(cursor(12, 5, true, 0))
        );

        // 光标落在区域外（无历史光标）：不揭示为 None，揭示回退到区域左上角。
        let narrow = PaneHostCursorInputs {
            area: Rect::new(10, 3, 2, 8),
            ..plain
        };
        assert_eq!(pane_host_cursor(&runtime, narrow), None);
        assert_eq!(
            pane_host_cursor(
                &runtime,
                PaneHostCursorInputs {
                    reveal: true,
                    ..narrow
                }
            ),
            Some(cursor(10, 3, true, 6))
        );
    }
}
