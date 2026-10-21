//! 手动视觉画廊：`just ui-gallery` 把典型场景合成成带颜色的 HTML，供界面改版逐帧审阅。
//! 不参与常规测试（`#[ignore]`），输出写到 `target/ui-gallery/`。

use super::*;
use crate::api::schema::{AgentActivityKind, AgentActivityStatus, ClientViewSpec};
use crate::client::endpoint::ClientEndpointId;
use crate::protocol::{
    ClientShellActivityNode, ClientShellAgentActivity, ClientShellExternalAgent, ServerMessage,
};
use ratatui::style::{Color, Modifier, Style};
use std::fmt::Write as _;
use std::time::{Duration, Instant};

const GENERATION: u64 = 1;
const REVISION: u64 = 7;

/// 画廊里打开某个浮层的动作。
type OpenOverlay = fn(&mut ClientShellState);

/// 画廊场景在第一帧之后改的状态（终端组与命中区要先合成一帧才有）。
type Prepare = fn(&mut ClientShellState);

/// 一帧画廊输出：标题、合成结果与它所在的宿主终端配色。
struct Shot {
    title: String,
    frame: FrameData,
    host: Host,
}

impl Shot {
    /// 深色宿主上的一帧（画廊默认）。
    fn dark(title: String, frame: FrameData) -> Self {
        Self {
            title,
            frame,
            host: DARK_HOST,
        }
    }
}

/// 宿主终端的默认前景 / 背景（`Color::Reset` 落在这里）。
#[derive(Clone, Copy)]
struct Host {
    fg: (u8, u8, u8),
    bg: (u8, u8, u8),
}

const DARK_HOST: Host = Host {
    fg: (0xcc, 0xcc, 0xcc),
    bg: (0x0c, 0x0c, 0x0c),
};

/// 浅色宿主（浅色主题的画面画在它上面）。
const LIGHT_HOST: Host = Host {
    fg: (0x38, 0x3a, 0x42),
    bg: (0xfa, 0xfa, 0xfa),
};

// 夹具构造：参数与画廊里各 agent 不同的字段一一对应，再包一层结构体只是搬运。
#[allow(clippy::too_many_arguments)]
fn agent(
    pane: &str,
    workspace: &str,
    tab: &str,
    name: &str,
    title: &str,
    status: AgentStatus,
    focused: bool,
    launch_seq: u64,
    activity: ClientShellAgentActivity,
) -> ClientShellAgent {
    ClientShellAgent {
        insight: crate::protocol::ClientShellAgentInsight {
            session_title: Some(title.into()),
            model: Some(
                if name == "codex" {
                    "gpt-6.1-sol"
                } else {
                    "Opus"
                }
                .into(),
            ),
            working_since_ms: Some(
                crate::harness::time::system_time_ms(std::time::SystemTime::now())
                    .unwrap_or_default()
                    .saturating_sub(185_000),
            ),
            last_turn_ms: Some(125_000),
            updated_at_ms: Some(
                crate::harness::time::system_time_ms(std::time::SystemTime::now())
                    .unwrap_or_default()
                    .saturating_sub(300_000),
            ),
            current_tool: (status == AgentStatus::Working)
                .then(|| "Edit src/client/shell/input.rs".into()),
            last_message: Some("身份隔离已补齐，正在核对布局和活动读取。".into()),
            last_prompt: Some("修复断线重连后的焦点与旧响应污染。".into()),
            question: (status == AgentStatus::Blocked)
                .then(|| "允许执行 cargo nextest 并修改 src/client/shell.rs 吗？".into()),
            options: if status == AgentStatus::Blocked {
                vec!["允许一次".into(), "拒绝".into()]
            } else {
                Vec::new()
            },
            ..Default::default()
        },
        pane_id: pane.into(),
        workspace_id: workspace.into(),
        tab_id: tab.into(),
        name: Some(name.into()),
        display_agent: Some(name.into()),
        agent: Some(name.into()),
        title: Some(title.into()),
        terminal_title: Some(title.into()),
        terminal_title_stripped: Some(title.into()),
        agent_status: status,
        state_change_seq: launch_seq,
        state_labels: Vec::new(),
        tokens: Vec::new(),
        focused,
        launch_seq,
        activity,
    }
}

fn node(
    id: &str,
    kind: AgentActivityKind,
    label: &str,
    status: AgentActivityStatus,
    parent: Option<&str>,
) -> ClientShellActivityNode {
    ClientShellActivityNode {
        id: id.into(),
        kind,
        label: label.into(),
        status,
        parent_id: parent.map(str::to_owned),
        agent_type: None,
        content_ref: None,
        summary: None,
        started_at_ms: None,
        ended_at_ms: None,
    }
}

fn workspace(
    id: &str,
    number: usize,
    label: &str,
    branch: Option<&str>,
    ahead_behind: Option<(usize, usize)>,
    status: AgentStatus,
    focused: bool,
) -> ClientShellWorkspace {
    ClientShellWorkspace {
        workspace_id: id.into(),
        active_tab_id: format!("{id}-t1"),
        new_workspace_cwd: format!("/src/{label}"),
        number,
        label: label.into(),
        custom_label: false,
        branch: branch.map(str::to_owned),
        git_ahead_behind: ahead_behind,
        tokens: Vec::new(),
        worktree: None,
        focused,
        agent_status: status,
    }
}

fn tab(
    id: &str,
    workspace: &str,
    number: usize,
    label: &str,
    status: AgentStatus,
    focused: bool,
) -> ClientShellTab {
    ClientShellTab {
        tab_id: id.into(),
        workspace_id: workspace.into(),
        number,
        label: label.into(),
        custom_label: label.parse::<usize>().is_err(),
        zoomed: false,
        focused,
        agent_status: status,
    }
}

fn pane(id: &str, workspace: &str, tab: &str, focused: bool) -> ClientShellPane {
    ClientShellPane {
        pane_id: id.into(),
        workspace_id: workspace.into(),
        tab_id: tab.into(),
        label: None,
        cwd: Some("/src/herdr".into()),
        foreground_cwd: Some("/src/herdr".into()),
        focused,
        right_click_passthrough: false,
    }
}

/// 典型多工作区场景：各状态的 agent、活动树、worktree、外部来源。
fn gallery_snapshot() -> ClientShellSnapshot {
    let mut snapshot = snapshot();
    snapshot.revision = REVISION;
    snapshot.focused_workspace_id = Some("ws1".into());
    snapshot.focused_tab_id = Some("ws1-t1".into());
    snapshot.focused_pane_id = Some("p1".into());
    snapshot.tab_bar_right = vec![crate::protocol::ClientShellTabStatusSegment {
        text: "devbox 14:02".into(),
        accent: false,
    }];
    let mut web = workspace(
        "ws3",
        3,
        "web-app",
        Some("fix/login-redirect"),
        Some((0, 3)),
        AgentStatus::Blocked,
        false,
    );
    web.worktree = Some(crate::protocol::ClientShellWorktree {
        key: "web".into(),
        label: "web-app".into(),
        is_linked_worktree: true,
    });
    snapshot.workspaces = vec![
        workspace(
            "ws1",
            1,
            "herdr",
            Some("feature/gx_herdr"),
            Some((2, 0)),
            AgentStatus::Working,
            true,
        ),
        workspace(
            "ws2",
            2,
            "gx_paseo",
            Some("main"),
            None,
            AgentStatus::Done,
            false,
        ),
        web,
        workspace("ws4", 4, "notes", None, None, AgentStatus::Idle, false),
    ];
    snapshot.tabs = vec![
        tab("ws1-t1", "ws1", 1, "1", AgentStatus::Working, true),
        tab("ws1-t2", "ws1", 2, "build", AgentStatus::Unknown, false),
        tab("ws2-t1", "ws2", 1, "1", AgentStatus::Done, false),
        tab("ws3-t1", "ws3", 1, "1", AgentStatus::Blocked, false),
        tab("ws4-t1", "ws4", 1, "1", AgentStatus::Idle, false),
    ];
    snapshot.panes = vec![
        pane("p1", "ws1", "ws1-t1", true),
        pane("p2", "ws1", "ws1-t1", false),
        pane("p3", "ws1", "ws1-t2", false),
        pane("p4", "ws2", "ws2-t1", false),
        pane("p5", "ws3", "ws3-t1", false),
        pane("p6", "ws4", "ws4-t1", false),
    ];
    let claude_activity = ClientShellAgentActivity {
        running: 2,
        total: 5,
        truncated: false,
        nodes: vec![
            node(
                "a1",
                AgentActivityKind::Subagent,
                "Explore 检测模块",
                AgentActivityStatus::Running,
                None,
            ),
            node(
                "a2",
                AgentActivityKind::Background,
                "cargo nextest run",
                AgentActivityStatus::Running,
                None,
            ),
        ],
        active: 2,
        done: 3,
        failed: 0,
    };
    snapshot.agents = vec![
        agent(
            "p1",
            "ws1",
            "ws1-t1",
            "claude",
            "修复窗格焦点丢失",
            AgentStatus::Working,
            true,
            1,
            claude_activity,
        ),
        agent(
            "p3",
            "ws1",
            "ws1-t2",
            "codex",
            "迁移 API 客户端重试",
            AgentStatus::Blocked,
            false,
            2,
            ClientShellAgentActivity::default(),
        ),
        agent(
            "p4",
            "ws2",
            "ws2-t1",
            "kimi",
            "整理 CLI 参数文档",
            AgentStatus::Done,
            false,
            3,
            ClientShellAgentActivity::default(),
        ),
        agent(
            "p5",
            "ws3",
            "ws3-t1",
            "opencode",
            "登录跳转回归",
            AgentStatus::Blocked,
            false,
            4,
            ClientShellAgentActivity::default(),
        ),
        agent(
            "p6",
            "ws4",
            "ws4-t1",
            "pi",
            "周报草稿",
            AgentStatus::Idle,
            false,
            5,
            ClientShellAgentActivity::default(),
        ),
    ];
    snapshot.external_agents = vec![ClientShellExternalAgent {
        external_id: "zcode:01".into(),
        source: "zcode".into(),
        agent_status: AgentStatus::Working,
        label: "重构数据管线".into(),
        readable: true,
        agent: Some("zcode".into()),
        cwd: Some("/src/pipeline".into()),
        updated_at_ms: None,
        activity: ClientShellAgentActivity::default(),
    }];
    snapshot
}

/// 模拟 agent 自己的 TUI 画面（真实画面由 server 渲染，这里只求有代表性）。颜色取主题
/// 色板，深浅宿主都读得清。
fn paint_agent_screen(buffer: &mut Buffer, area: Rect, agent: Option<&str>, palette: &Palette) {
    let dim = Style::default().fg(palette.overlay1);
    let text = Style::default().fg(palette.text);
    let accent = Style::default().fg(palette.peach);
    let green = Style::default().fg(palette.green);
    let lines: Vec<(Style, String)> = match agent {
        Some("claude") => vec![
            (accent, "✻ Welcome to Claude Code".into()),
            (
                dim,
                "  /help for help, /status for your current setup".into(),
            ),
            (text, String::new()),
            (text, "> 修复客户端在 SSH 断线后窗格焦点丢失的问题".into()),
            (text, String::new()),
            (green, "● Read(src/client/shell/input.rs)".into()),
            (dim, "  ⎿  Read 1299 lines".into()),
            (
                green,
                "● Search(pattern: \"focused_pane_id\", path: \"src\")".into(),
            ),
            (dim, "  ⎿  Found 42 files".into()),
            (green, "● Update(src/client/shell/input.rs)".into()),
            (dim, "  ⎿  Updated with 6 additions and 2 removals".into()),
            (text, String::new()),
            (
                accent,
                "✻ Pondering… (38s · ↓ 2.1k tokens · esc to interrupt)".into(),
            ),
        ],
        Some("codex") => vec![
            (text, ">_ OpenAI Codex (v0.142)".into()),
            (dim, "   model: gpt-6 high   directory: ~/src/herdr".into()),
            (text, String::new()),
            (text, "› 把 API 客户端迁移到 fetch 并加上重试".into()),
            (text, String::new()),
            (dim, "• Explored".into()),
            (dim, "  └ Read client.ts, retry.ts".into()),
            (text, String::new()),
            (accent, "  Allow command?".into()),
            (text, "  $ npm test -- --run src/api".into()),
            (text, "  › 1. Yes, proceed".into()),
            (
                dim,
                "    2. Yes, and don't ask again for this command".into(),
            ),
            (
                dim,
                "    3. No, and tell Codex what to do differently".into(),
            ),
        ],
        Some(_) => vec![
            (text, "agent ready".into()),
            (dim, "Type a message…".into()),
        ],
        None => vec![
            (green, "~/src/herdr feature/gx_herdr ↑2".into()),
            (text, "❯ cargo nextest run -p herdr client::shell".into()),
            (
                dim,
                "    Finished `test` profile [unoptimized] in 41.2s".into(),
            ),
            (
                text,
                "     Summary [  38.104s] 935 tests run: 935 passed".into(),
            ),
            (green, "~/src/herdr feature/gx_herdr ↑2".into()),
            (text, "❯ ".into()),
        ],
    };
    for (row, (style, line)) in lines.iter().enumerate() {
        let y = area.y.saturating_add(row as u16);
        if y >= area.bottom() {
            break;
        }
        let x = area.x;
        let max = usize::from(area.width);
        buffer.set_stringn(x, y, line, max, *style);
    }
}

/// 按视图规格合成一帧模拟的终端组画面：单窗格铺满；两个窗格左右拆分并画边框。边框
/// 字形、颜色与标题和 server 同一口径（`ui.border_style`、窗格边框 token、agent 窗格
/// 标题 ` ◆ 名称 ` 画静态状态字形）。
fn mock_surface(
    spec: &ClientViewSpec,
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
) -> PaneSurfaceFrame {
    let area = Rect::new(0, 0, spec.cols, spec.rows);
    let mut buffer = Buffer::empty(area);
    let panes = snapshot
        .panes
        .iter()
        .filter(|pane| pane.tab_id == spec.tab_id)
        .collect::<Vec<_>>();
    let agent_of = |pane_id: &str| {
        snapshot
            .agents
            .iter()
            .find(|agent| agent.pane_id == pane_id)
    };
    let agent_name = |pane_id: &str| agent_of(pane_id).and_then(|agent| agent.agent.as_deref());
    let mut surface_panes = Vec::new();
    if panes.len() >= 2 {
        let left_width = spec.cols / 2;
        let rects = [
            Rect::new(0, 0, left_width, spec.rows),
            Rect::new(left_width, 0, spec.cols - left_width, spec.rows),
        ];
        for (pane, rect) in panes.iter().take(2).zip(rects) {
            let color = if pane.focused {
                config.components.pane_border_focused
            } else {
                config.components.pane_border_unfocused
            };
            ratatui::widgets::Widget::render(
                ratatui::widgets::Block::bordered()
                    .border_set(config.border_glyphs.border_set())
                    .border_style(Style::default().fg(color)),
                rect,
                &mut buffer,
            );
            if let Some(agent) = agent_of(&pane.pane_id) {
                let bucket = crate::client::shell::client_agent_bucket(agent);
                let glyph = (bucket != crate::harness::AgentBucket::None).then(|| {
                    (
                        crate::ui::design::status_glyph(
                            bucket,
                            crate::config::StatusIndicatorStyle::Graphite,
                            0,
                            false,
                        ),
                        crate::ui::design::status_style(
                            bucket,
                            &config.palette,
                            &config.components,
                        ),
                    )
                });
                let mut style = Style::default().fg(color);
                if pane.focused {
                    style = style.add_modifier(Modifier::BOLD);
                }
                crate::ui::paint_pane_title(
                    &mut buffer,
                    (
                        rect.x + crate::ui::PANE_TITLE_INSET,
                        rect.right().saturating_sub(1),
                        rect.y,
                    ),
                    glyph,
                    &format!(" {} ", agent.agent.as_deref().unwrap_or_default()),
                    style,
                );
            }
            let inner = Rect::new(
                rect.x + 1,
                rect.y + 1,
                rect.width.saturating_sub(2),
                rect.height.saturating_sub(2),
            );
            paint_agent_screen(
                &mut buffer,
                inner,
                agent_name(&pane.pane_id),
                &config.palette,
            );
            surface_panes.push(surface_pane(&pane.pane_id, rect, inner, pane.focused));
        }
    } else if let Some(pane) = panes.first() {
        paint_agent_screen(
            &mut buffer,
            area,
            agent_name(&pane.pane_id),
            &config.palette,
        );
        surface_panes.push(surface_pane(&pane.pane_id, area, area, pane.focused));
    }
    PaneSurfaceFrame {
        boot_id: snapshot.boot_id.clone(),
        projection_revision: snapshot.revision,
        surface_revision: 1,
        frame: FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[]),
        panes: surface_panes,
        splits: Vec::new(),
        popup: None,
        graphics: crate::protocol::SurfaceGraphicsScene::default(),
    }
}

fn surface_rect(rect: Rect) -> SurfaceRect {
    SurfaceRect {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
    }
}

fn surface_pane(pane_id: &str, rect: Rect, inner: Rect, focused: bool) -> PaneSurfacePane {
    PaneSurfacePane {
        pane_id: pane_id.into(),
        content_revision: 0,
        rect: surface_rect(rect),
        inner_rect: surface_rect(inner),
        scrollbar_rect: None,
        scroll: None,
        focused,
        mouse_reporting: false,
        sgr_pixel_mouse: false,
        alternate_screen_active: false,
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// 工作台状态：宣告 `client.views.set`，快照走代际 1。
fn workbench_state(config: ClientShellConfig) -> ClientShellState {
    let mut state = ClientShellState::new(config);
    state.set_endpoint_snapshot_for_generation(
        &ClientEndpointId::Local,
        GENERATION,
        Box::new(gallery_snapshot()),
    );
    state.set_endpoint_methods(Some(vec![
        "client.views.set".into(),
        "tab.focus".into(),
        "agent.activity.read".into(),
    ]));
    state
}

/// 合成一帧：先按尺寸计算视图请求，模拟 server 应答各视图，再合成最终画面。
fn shoot_workbench(
    state: &mut ClientShellState,
    cols: u16,
    rows: u16,
    step: &mut u64,
) -> FrameData {
    let _ = state.compose(cols, rows);
    *step += 1;
    let now = Instant::now() + Duration::from_secs(*step);
    state.tick_workbench(now, &mut ClientShellInput::default());
    state.workbench.pending = false;
    state.workbench.acknowledged = state.workbench.revision;
    let snapshot = state.snapshot.as_deref().cloned().expect("snapshot");
    for spec in state.workbench.requested.clone() {
        let surface = mock_surface(&spec, &snapshot, &state.config);
        let view = crate::protocol::views::DecodedView {
            boot_id: snapshot.boot_id.clone(),
            views_revision: state.workbench.revision,
            view_id: spec.view_id.clone(),
            tab_id: spec.tab_id.clone(),
            message: ServerMessage::PaneSurface(surface),
        };
        state.receive_view(GENERATION, view);
    }
    let mut activity = ClientShellInput::default();
    state.tick_inspector(now, &mut activity);
    let reads = state
        .pending_requests
        .iter()
        .filter(|(_, pending)| matches!(pending.kind, PendingEndpointKind::InspectorRead { .. }))
        .map(|(id, pending)| (id.clone(), pending.boot_id.clone()))
        .collect::<Vec<_>>();
    for (request_id, boot_id) in reads {
        let nodes = (0..8)
            .map(|index| crate::api::schema::AgentActivityNode {
                id: format!("detail-{index}"),
                label: [
                    "Explore 检测模块",
                    "Read src/detect/mod.rs",
                    "Edit src/client/shell.rs",
                    "Run cargo nextest",
                    "Check translations",
                    "Review geometry",
                    "Validate target race",
                    "Background build",
                ][index]
                    .into(),
                parent_id: (index == 1 || index == 2).then(|| "detail-0".into()),
                kind: if index == 0 {
                    AgentActivityKind::Subagent
                } else {
                    AgentActivityKind::Task
                },
                status: if index < 2 {
                    AgentActivityStatus::Running
                } else {
                    AgentActivityStatus::Done
                },
                ..Default::default()
            })
            .collect();
        state.handle_endpoint_result(
            &boot_id,
            &request_id,
            Ok(crate::api::schema::ResponseResult::AgentActivity {
                nodes,
                content: None,
            }),
        );
    }
    state.compose(cols, rows).expect("画廊帧").frame.clone()
}

fn classic_state(config: ClientShellConfig) -> ClientShellState {
    let mut state = ClientShellState::new(config);
    state.set_endpoint_snapshot_for_generation(
        &ClientEndpointId::Local,
        GENERATION,
        Box::new(gallery_snapshot()),
    );
    state
}

fn shoot_classic(state: &mut ClientShellState, cols: u16, rows: u16) -> FrameData {
    let snapshot = state.snapshot.as_deref().cloned().expect("snapshot");
    let spec = ClientViewSpec {
        view_id: "classic".into(),
        tab_id: snapshot.focused_tab_id.clone().unwrap_or_default(),
        cols: cols.saturating_sub(26),
        rows: rows.saturating_sub(1),
        focused: true,
    };
    let surface = mock_surface(&spec, &snapshot, &state.config);
    state.set_pane_surface(surface);
    state.compose(cols, rows).expect("经典布局帧").frame.clone()
}

fn rgb(color: Color, fallback: (u8, u8, u8)) -> (u8, u8, u8) {
    const ANSI: [(u8, u8, u8); 16] = [
        (0x0c, 0x0c, 0x0c),
        (0xc5, 0x0f, 0x1f),
        (0x13, 0xa1, 0x0e),
        (0xc1, 0x9c, 0x00),
        (0x00, 0x37, 0xda),
        (0x88, 0x17, 0x98),
        (0x3a, 0x96, 0xdd),
        (0xcc, 0xcc, 0xcc),
        (0x76, 0x76, 0x76),
        (0xe7, 0x48, 0x56),
        (0x16, 0xc6, 0x0c),
        (0xf9, 0xf1, 0xa5),
        (0x3b, 0x78, 0xff),
        (0xb4, 0x00, 0x9e),
        (0x61, 0xd6, 0xd6),
        (0xf2, 0xf2, 0xf2),
    ];
    match color {
        Color::Reset => fallback,
        Color::Black => ANSI[0],
        Color::Red => ANSI[1],
        Color::Green => ANSI[2],
        Color::Yellow => ANSI[3],
        Color::Blue => ANSI[4],
        Color::Magenta => ANSI[5],
        Color::Cyan => ANSI[6],
        Color::Gray => ANSI[7],
        Color::DarkGray => ANSI[8],
        Color::LightRed => ANSI[9],
        Color::LightGreen => ANSI[10],
        Color::LightYellow => ANSI[11],
        Color::LightBlue => ANSI[12],
        Color::LightMagenta => ANSI[13],
        Color::LightCyan => ANSI[14],
        Color::White => ANSI[15],
        Color::Indexed(index) if index < 16 => ANSI[usize::from(index)],
        Color::Indexed(index) if index >= 232 => {
            let level = 8 + (index - 232) * 10;
            (level, level, level)
        }
        Color::Indexed(index) => {
            let index = index - 16;
            let step = |value: u8| if value == 0 { 0 } else { 55 + value * 40 };
            (step(index / 36), step((index / 6) % 6), step(index % 6))
        }
        Color::Rgb(r, g, b) => (r, g, b),
    }
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// 帧 → HTML：每格定宽（宽字符占两格），颜色与粗细 / 暗淡 / 下划线 / 反色按格还原。
fn frame_html(frame: &FrameData, host: Host) -> String {
    let mut html = String::new();
    let width = usize::from(frame.width);
    for row in frame.cells.chunks(width.max(1)) {
        html.push_str("<div class=\"row\">");
        let mut skip = 0usize;
        for cell in row {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let symbol = if cell.symbol.is_empty() {
                " "
            } else {
                cell.symbol.as_str()
            };
            let cells = crate::ui::display_width(symbol).clamp(1, 2);
            skip = cells - 1;
            let modifier = Modifier::from_bits_truncate(cell.modifier);
            let mut fg = rgb(crate::protocol::u32_to_color(cell.fg), host.fg);
            let mut bg = rgb(crate::protocol::u32_to_color(cell.bg), host.bg);
            if modifier.contains(Modifier::REVERSED) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if modifier.contains(Modifier::DIM) {
                fg = (
                    ((u16::from(fg.0) + u16::from(bg.0)) / 2) as u8,
                    ((u16::from(fg.1) + u16::from(bg.1)) / 2) as u8,
                    ((u16::from(fg.2) + u16::from(bg.2)) / 2) as u8,
                );
            }
            let mut css = format!(
                "width:{cells}ch;color:#{:02x}{:02x}{:02x};background:#{:02x}{:02x}{:02x}",
                fg.0, fg.1, fg.2, bg.0, bg.1, bg.2
            );
            if modifier.contains(Modifier::BOLD) {
                css.push_str(";font-weight:700");
            }
            if modifier.contains(Modifier::ITALIC) {
                css.push_str(";font-style:italic");
            }
            if modifier.contains(Modifier::UNDERLINED) {
                css.push_str(";text-decoration:underline");
            }
            if modifier.contains(Modifier::CROSSED_OUT) {
                css.push_str(";text-decoration:line-through");
            }
            if modifier.contains(Modifier::HIDDEN) {
                css.push_str(";color:transparent");
            }
            let _ = write!(html, "<span style=\"{css}\">{}</span>", escape(symbol));
        }
        html.push_str("</div>");
    }
    html
}

fn page(shots: &[Shot]) -> String {
    let mut html = String::from(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>herdr ui gallery</title><style>\
         body{background:#2b2b2b;color:#ddd;font-family:system-ui,sans-serif;margin:16px}\
         h2{font-size:14px;font-weight:600;margin:24px 0 8px}\
         .term{display:inline-block;font-family:'Cascadia Mono','Sarasa Mono SC','Microsoft YaHei Mono',Consolas,monospace;\
         font-size:13px;line-height:17px;box-shadow:0 4px 24px rgba(0,0,0,.5)}\
         .row{height:17px;display:flex;white-space:pre}.row span{display:inline-block;flex:none;overflow:hidden;text-align:left}\
         </style></head><body>\n",
    );
    for shot in shots {
        let _ = write!(
            html,
            "<h2 id=\"{}\">{} — {}×{}</h2>\n<div class=\"term\" style=\"background:#{:02x}{:02x}{:02x}\">{}</div>\n",
            escape(&shot.title),
            escape(&shot.title),
            shot.frame.width,
            shot.frame.height,
            shot.host.bg.0,
            shot.host.bg.1,
            shot.host.bg.2,
            frame_html(&shot.frame, shot.host)
        );
    }
    html.push_str("</body></html>\n");
    html
}

fn gallery_dir() -> std::path::PathBuf {
    std::env::var_os("HERDR_UI_GALLERY_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join("ui-gallery")
        })
}

fn write_shots(name: &str, shots: &[Shot]) {
    let dir = gallery_dir();
    std::fs::create_dir_all(&dir).expect("gallery dir");
    std::fs::write(dir.join(format!("{name}.html")), page(shots)).expect("gallery html");
    for shot in shots {
        let text = frame_rows(&shot.frame).join("\n");
        let file = shot
            .title
            .chars()
            .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
            .collect::<String>();
        std::fs::write(dir.join(format!("{name}-{file}.txt")), text).expect("gallery text");
        std::fs::write(
            dir.join(format!("{name}-{file}.html")),
            page(std::slice::from_ref(shot)),
        )
        .expect("gallery shot html");
    }
}

/// 先按尺寸合成一帧（建立终端组与命中区），再改状态、重新应答视图后合成。
fn shoot_scene(
    config: ClientShellConfig,
    (cols, rows): (u16, u16),
    prepare: Prepare,
    step: &mut u64,
) -> FrameData {
    let mut state = workbench_state(config);
    let _ = shoot_workbench(&mut state, cols, rows, step);
    prepare(&mut state);
    shoot_workbench(&mut state, cols, rows, step)
}

#[test]
#[ignore = "manual visual gallery; run `just ui-gallery`"]
fn ui_gallery() {
    use crate::client::shell::dock::PanelId;
    let _lang = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    let config = || ClientShellConfig::from_config(&Config::default());

    let mut shots = Vec::new();
    let mut step = 0;
    for (cols, rows) in [
        (200, 50),
        (160, 45),
        (120, 40),
        (100, 30),
        (80, 24),
        (60, 20),
    ] {
        let mut state = workbench_state(config());
        let frame = shoot_workbench(&mut state, cols, rows, &mut step);
        shots.push(Shot::dark(format!("workbench {cols}x{rows}"), frame));
    }

    // 工作台的几种状态：停靠监控面板（多个面板头与关闭按钮）、调整布局、最大化、
    // 紧凑视图（停靠监控面板后 60 列放不下）、前缀模式的状态栏。
    let scenes: [(&str, (u16, u16), Prepare); 5] = [
        ("monitor docked", (200, 50), |state| {
            state.workbench_open(PanelId::Monitor);
        }),
        ("arrange", (160, 45), |state| {
            state.workbench.arranging = true
        }),
        ("maximized", (160, 45), |state| {
            let terminal = state
                .workbench
                .dock
                .groups
                .first()
                .map(|group| PanelId::Terminal(group.id));
            state.workbench.dock.maximized = terminal;
        }),
        ("compact", (60, 20), |state| {
            state.workbench_open(PanelId::Monitor);
        }),
        ("prefix mode", (160, 45), |state| {
            state.mode = ClientShellMode::Prefix;
        }),
    ];
    for (name, size, prepare) in scenes {
        let frame = shoot_scene(config(), size, prepare, &mut step);
        shots.push(Shot::dark(format!("{name} {}x{}", size.0, size.1), frame));
    }

    // 菜单按「点入口」的方式打开（不预选首项）；锁定那一帧再把指针停在「锁定布局」上，
    // 看勾选、置灰的复位与悬浮底色。
    let overlays: [(&str, OpenOverlay); 8] = [
        ("main menu", |state| {
            state.toggle_global_menu();
            state.release_catalog_selection();
        }),
        ("layout menu", |state| {
            state.toggle_layout_menu();
            state.release_catalog_selection();
        }),
        ("layout menu locked", |state| {
            state.workbench.dock.locked = true;
            state.toggle_layout_menu();
            state.release_catalog_selection();
            let lock = palette_row_index(state, "layout:lock");
            state.set_palette_hover(Some(lock));
        }),
        ("palette", |state| state.open_command_search()),
        ("navigator", |state| state.open_navigator_overlay()),
        ("settings", |state| state.open_settings_overlay()),
        ("notifications", |state| state.open_notification_history()),
        ("help", |state| {
            state.overlay = Some(ClientShellOverlay::Help(ClientHelpOverlay {
                query: TextEditor::default(),
                search_focused: false,
                scroll: 0,
                max_scroll: 0,
            }));
        }),
    ];
    for (name, open) in overlays {
        let mut state = workbench_state(config());
        let _ = shoot_workbench(&mut state, 160, 45, &mut step);
        open(&mut state);
        let frame = state.compose(160, 45).expect("浮层帧").frame.clone();
        shots.push(Shot::dark(format!("overlay {name}"), frame));
    }

    // 浅色主题：graphite-light 画在浅色宿主上。
    let mut light = Config::default();
    light.theme.name = Some("graphite-light".into());
    let light_scenes: [(&str, (u16, u16), Prepare); 2] = [
        ("graphite-light", (160, 45), |_| {}),
        ("graphite-light monitor docked", (200, 50), |state| {
            state.workbench_open(PanelId::Monitor);
        }),
    ];
    for (name, size, prepare) in light_scenes {
        let frame = shoot_scene(
            ClientShellConfig::from_config(&light),
            size,
            prepare,
            &mut step,
        );
        shots.push(Shot {
            title: format!("{name} {}x{}", size.0, size.1),
            frame,
            host: LIGHT_HOST,
        });
    }

    for (cols, rows) in [(140, 40), (60, 24)] {
        let mut state = classic_state(config());
        let frame = shoot_classic(&mut state, cols, rows);
        shots.push(Shot::dark(format!("classic {cols}x{rows}"), frame));
    }
    // 经典布局的模式条与工作台状态栏是同一个组件：前缀模式压在终端区最后一行。
    let mut state = classic_state(config());
    state.mode = ClientShellMode::Prefix;
    let frame = shoot_classic(&mut state, 140, 40);
    shots.push(Shot::dark("classic prefix 140x40".into(), frame));

    for (name, pane, size, light_theme) in [
        ("inspector working", "p1", (200, 50), false),
        ("inspector needs-input", "p3", (160, 45), false),
        ("inspector failed", "p5", (160, 45), false),
        ("inspector idle", "p6", (160, 45), false),
        ("inspector plain", "p2", (160, 45), false),
        ("inspector narrow-i", "p3", (120, 40), false),
        ("inspector graphite-light", "p3", (200, 50), true),
    ] {
        let config = if light_theme {
            ClientShellConfig::from_config(&light)
        } else {
            config()
        };
        let mut state = workbench_state(config);
        let _ = shoot_workbench(&mut state, size.0, size.1, &mut step);
        let mut next = gallery_snapshot();
        next.revision += 1;
        next.focused_pane_id = Some(pane.into());
        for entry in &mut next.panes {
            entry.focused = entry.pane_id == pane;
        }
        for entry in &mut next.agents {
            entry.focused = entry.pane_id == pane;
        }
        if let Some(selected) = next.panes.iter().find(|entry| entry.pane_id == pane) {
            next.focused_tab_id = Some(selected.tab_id.clone());
            next.focused_workspace_id = Some(selected.workspace_id.clone());
        }
        if name == "inspector failed" {
            let agent = next
                .agents
                .iter_mut()
                .find(|entry| entry.pane_id == pane)
                .expect("failed agent");
            agent.agent_status = AgentStatus::Idle;
            agent.insight.error = Some("测试命令退出码 1：活动读取的目标已经失效。".into());
            agent.insight.attention = Some(crate::protocol::ClientShellAgentAttention {
                kind: crate::api::schema::AgentAttentionKind::Failed,
                ..Default::default()
            });
        }
        state.set_endpoint_snapshot_for_generation(
            &ClientEndpointId::Local,
            GENERATION,
            Box::new(next),
        );
        state.workbench.inspector_visibility.insert(
            "local".into(),
            crate::client::shell::dock::InspectorVisibility::Shown,
        );
        let frame = shoot_workbench(&mut state, size.0, size.1, &mut step);
        shots.push(Shot {
            title: format!("{name} {}x{}", size.0, size.1),
            frame,
            host: if light_theme { LIGHT_HOST } else { DARK_HOST },
        });
        if name == "inspector working" {
            let mut timeline_snapshot = state
                .snapshot
                .as_deref()
                .cloned()
                .expect("timeline snapshot");
            for turn in 0..10 {
                timeline_snapshot.revision += 1;
                timeline_snapshot.agents[0].agent_status = if turn % 2 == 0 {
                    AgentStatus::Blocked
                } else {
                    AgentStatus::Working
                };
                timeline_snapshot.agents[0].insight.state_changed_at_ms = Some(
                    crate::harness::time::system_time_ms(std::time::SystemTime::now())
                        .unwrap_or_default()
                        .saturating_sub((10 - turn) * 60_000),
                );
                state.set_endpoint_snapshot_for_generation(
                    &ClientEndpointId::Local,
                    GENERATION,
                    Box::new(timeline_snapshot.clone()),
                );
            }
            state.workbench.dock.focused = PanelId::Agents;
            state.handle_raw_events(vec![RawInputEvent::Key(crate::input::TerminalKey::new(
                KeyCode::End,
                KeyModifiers::NONE,
            ))]);
            shots.push(Shot::dark(
                "inspector timeline 200x50".into(),
                shoot_workbench(&mut state, 200, 50, &mut step),
            ));
        }
    }
    let mut not_reported = workbench_state(config());
    let _ = shoot_workbench(&mut not_reported, 200, 50, &mut step);
    let opened = not_reported.handle_input_bytes(b"\x02i");
    let (boot_id, request_id) = opened
        .actions
        .iter()
        .find_map(|action| match action {
            ClientShellAction::EndpointRequest {
                boot_id, request, ..
            } if matches!(
                request.method,
                crate::api::schema::Method::AgentActivityRead(_)
            ) =>
            {
                Some((boot_id, &request.id))
            }
            _ => None,
        })
        .expect("explicit Inspector activity read");
    assert!(
        not_reported
            .handle_endpoint_result(
                boot_id,
                request_id,
                Err(ClientShellEndpointError {
                    code: Some("not_implemented".into()),
                    message: "agent activity is not implemented by this server".into(),
                }),
            )
            .0
    );
    shots.push(Shot::dark(
        "inspector not-reported 200x50".into(),
        shoot_workbench(&mut not_reported, 200, 50, &mut step),
    ));

    let mut migrated_config = config();
    let mut legacy = crate::client::shell::dock::DockLayout::default();
    legacy.dock(
        PanelId::Agents,
        &PanelId::Workspaces,
        crate::client::shell::dock::Edge::Bottom,
    );
    legacy.version = 1;
    migrated_config
        .preferences
        .layouts
        .insert("local".into(), legacy);
    migrated_config.preferences.inspector_visibility.insert(
        "local".into(),
        crate::client::shell::dock::InspectorVisibility::Shown,
    );
    let mut migrated = workbench_state(migrated_config);
    let frame = shoot_workbench(&mut migrated, 200, 50, &mut step);
    shots.push(Shot::dark("inspector v1 migration 200x50".into(), frame));

    for theme in ["graphite-dark", "graphite-light"] {
        for (cols, rows) in [(160, 45), (80, 24), (60, 20)] {
            let mut themed = Config::default();
            themed.theme.name = Some(theme.into());
            let host = if theme == "graphite-light" {
                LIGHT_HOST
            } else {
                DARK_HOST
            };
            for section in ClientSettingsSection::ALL {
                let mut state = workbench_state(ClientShellConfig::from_config(&themed));
                let _ = shoot_workbench(&mut state, cols, rows, &mut step);
                state.open_settings_overlay();
                state.select_settings_section(*section, &mut ClientShellInput::default());
                if let Some(ClientShellOverlay::Settings(settings)) = state.overlay.as_mut() {
                    settings.focus = super::super::page::PageFocus::Content;
                    settings.loading_integrations = false;
                    if *section == ClientSettingsSection::Integrations {
                        settings.integrations = vec![
                            crate::api::schema::IntegrationInfo {
                                target: crate::api::schema::IntegrationTarget::Codex,
                                label: "codex".into(),
                                command: "codex".into(),
                                available: true,
                                state: crate::api::schema::IntegrationState::Outdated,
                            },
                            crate::api::schema::IntegrationInfo {
                                target: crate::api::schema::IntegrationTarget::Claude,
                                label: "claude".into(),
                                command: "claude".into(),
                                available: true,
                                state: crate::api::schema::IntegrationState::Current,
                            },
                        ];
                        settings.integration_messages = vec!["codex: installation complete; restart the agent to activate the updated hooks.".into()];
                    }
                }
                shots.push(Shot {
                    title: format!("settings {theme} {section:?} {cols}x{rows}"),
                    frame: state
                        .compose(cols, rows)
                        .expect("settings gallery")
                        .frame
                        .clone(),
                    host,
                });
            }
            let mut state = workbench_state(ClientShellConfig::from_config(&themed));
            let mut empty = gallery_snapshot();
            empty.workspaces.clear();
            empty.tabs.clear();
            empty.panes.clear();
            empty.agents.clear();
            empty.external_agents.clear();
            empty.focused_workspace_id = None;
            empty.focused_tab_id = None;
            empty.focused_pane_id = None;
            state.set_endpoint_snapshot_for_generation(
                &ClientEndpointId::Local,
                GENERATION,
                Box::new(empty),
            );
            shots.push(Shot {
                title: format!("welcome {theme} {cols}x{rows}"),
                frame: shoot_workbench(&mut state, cols, rows, &mut step),
                host,
            });
            let mut classic = classic_state(ClientShellConfig::from_config(&themed));
            classic.set_snapshot(Box::new(
                state.snapshot.as_deref().cloned().expect("empty snapshot"),
            ));
            shots.push(Shot {
                title: format!("welcome classic {theme} {cols}x{rows}"),
                frame: shoot_classic(&mut classic, cols, rows),
                host,
            });
            state.overlay = Some(ClientShellOverlay::Help(ClientHelpOverlay {
                query: TextEditor::default(),
                search_focused: false,
                scroll: 0,
                max_scroll: 0,
            }));
            shots.push(Shot {
                title: format!("help {theme} {cols}x{rows}"),
                frame: state
                    .compose(cols, rows)
                    .expect("help gallery")
                    .frame
                    .clone(),
                host,
            });
        }
    }
    for theme in ["graphite-dark", "graphite-light"] {
        for (cols, rows) in [(160, 45), (80, 24), (60, 20)] {
            let mut themed = Config::default();
            themed.theme.name = Some(theme.into());
            let host = if theme == "graphite-light" {
                LIGHT_HOST
            } else {
                DARK_HOST
            };
            let mut state = workbench_state(ClientShellConfig::from_config(&themed));
            let _ = shoot_workbench(&mut state, cols, rows, &mut step);
            state.config.feedback.animations = false;
            state.palette_recent = vec!["binding:Settings".into(), "binding:Help".into()];
            state.open_command_search();
            shots.push(Shot {
                title: format!("command sections {theme} {cols}x{rows}"),
                frame: state
                    .compose(cols, rows)
                    .expect("command sections")
                    .frame
                    .clone(),
                host,
            });
            if let Some(ClientShellOverlay::CommandPalette(palette)) = state.overlay.as_mut() {
                palette.query = TextEditor::new("settings", false);
                palette.selected = 0;
                palette.reveal = true;
            }
            shots.push(Shot {
                title: format!("command filtered keys {theme} {cols}x{rows}"),
                frame: state
                    .compose(cols, rows)
                    .expect("filtered command")
                    .frame
                    .clone(),
                host,
            });
            state.overlay = None;
            state.open_pane_context_menu("p1".into(), 4, 3);
            shots.push(Shot {
                title: format!("menu descriptions {theme} {cols}x{rows}"),
                frame: state
                    .compose(cols, rows)
                    .expect("menu descriptions")
                    .frame
                    .clone(),
                host,
            });
            state.overlay = None;
            state.workbench.dock.locked = true;
            state.toggle_layout_menu();
            shots.push(Shot {
                title: format!("menu checked disabled {theme} {cols}x{rows}"),
                frame: state
                    .compose(cols, rows)
                    .expect("layout disabled")
                    .frame
                    .clone(),
                host,
            });
            state.overlay = None;
            let profile = SavedSshEndpoint::new("Remote", "dev@example.com", "gallery")
                .expect("remote profile");
            let remote = ClientEndpointId::Ssh(profile.id.clone());
            state.set_endpoint_catalog(&[profile]);
            state.set_endpoint_status(&remote, ClientEndpointStatus::Online);
            state.set_endpoint_snapshot(&remote, Box::new(gallery_snapshot()));
            for (name, kind, sound, title) in [
                (
                    "needsinput",
                    SemanticNotificationKind::NeedsAttention,
                    None,
                    "codex 需要你的输入",
                ),
                (
                    "failed",
                    SemanticNotificationKind::Custom,
                    Some(SemanticNotificationSound::Request),
                    "codex 运行失败",
                ),
            ] {
                let body = if name == "failed" {
                    "远端工具运行失败：无法写入 src/client/shell/state.rs。请检查文件权限与路径，修复后重新运行任务。"
                } else {
                    "请确认 src/client/shell/state.rs 的修改。远端智能体正在等待本次操作；完整权限问题含中文和长路径。"
                };
                state.record_notification(
                    super::super::feedback::ClientToastLevel::Error,
                    title,
                    Some(body.into()),
                    Some(super::super::feedback::ClientNotificationTarget {
                        endpoint_id: remote.clone(),
                        pane_id: Some("p3".into()),
                        boot_id: Some("boot-1".into()),
                    }),
                );
                if let Some(record) = state.notification_history.back_mut() {
                    record.bucket = Some(if name == "failed" {
                        crate::harness::AgentBucket::Failed
                    } else {
                        crate::harness::AgentBucket::NeedsInput
                    });
                }
                state.visible_notification = Some(ClientVisibleNotification {
                    endpoint_id: remote.clone(),
                    event: SemanticNotification {
                        kind,
                        title: title.into(),
                        body: Some(body.into()),
                        sound,
                        agent: Some("codex".into()),
                        workspace_id: Some("ws1".into()),
                        tab_id: Some("ws1-t2".into()),
                        pane_id: Some("p3".into()),
                        position: Some(crate::config::ToastHerdrPosition::BottomRight),
                    },
                    deadline: Instant::now() + Duration::from_secs(30),
                });
                state.notification_focused = true;
                shots.push(Shot {
                    title: format!("notification {name} {theme} {cols}x{rows}"),
                    frame: state
                        .compose(cols, rows)
                        .expect("notification card")
                        .frame
                        .clone(),
                    host,
                });
            }
            state.visible_notification = None;
            state.notification_focused = false;
            state.open_notification_history();
            shots.push(Shot {
                title: format!("notification history body {theme} {cols}x{rows}"),
                frame: state
                    .compose(cols, rows)
                    .expect("history body")
                    .frame
                    .clone(),
                host,
            });
        }
    }
    for (name, pane, size, light_theme, reply, supported) in [
        ("overview status", "p1", (160, 45), false, false, true),
        (
            "overview permission preview",
            "p3",
            (160, 45),
            false,
            false,
            true,
        ),
        (
            "overview permission narrow",
            "p3",
            (80, 30),
            false,
            false,
            true,
        ),
        (
            "overview failed preview",
            "p5",
            (160, 45),
            false,
            false,
            true,
        ),
        ("overview reply", "p6", (160, 45), false, true, true),
        ("overview reply narrow", "p6", (60, 24), false, true, true),
        (
            "overview old-server disabled",
            "p6",
            (80, 30),
            false,
            true,
            false,
        ),
        ("overview graphite-light", "p6", (160, 45), true, true, true),
    ] {
        let cfg = if light_theme {
            ClientShellConfig::from_config(&light)
        } else {
            config()
        };
        let mut state = workbench_state(cfg);
        let _ = shoot_workbench(&mut state, size.0, size.1, &mut step);
        if supported {
            state.set_endpoint_methods(Some(vec![
                "client.views.set".into(),
                "pane.focus".into(),
                "agent.prompt".into(),
            ]));
        }
        let mut next = gallery_snapshot();
        next.revision += 1;
        {
            let agent = next
                .agents
                .iter_mut()
                .find(|agent| agent.pane_id == "p5")
                .expect("failed");
            agent.agent_status = AgentStatus::Idle;
            agent.insight.error = Some("退出码 1：活动读取目标已经失效。".into());
            agent.insight.attention = Some(crate::protocol::ClientShellAgentAttention {
                kind: crate::api::schema::AgentAttentionKind::Failed,
                ..Default::default()
            });
        }
        state.set_endpoint_snapshot_for_generation(
            &ClientEndpointId::Local,
            GENERATION,
            Box::new(next),
        );
        let review = state.snapshot.as_deref().and_then(|snapshot| {
            snapshot
                .agents
                .iter()
                .find(|agent| agent.pane_id == "p4")
                .map(|agent| {
                    crate::client::shell::chrome::inspector::Target::for_agent(
                        ClientEndpointId::Local,
                        snapshot,
                        agent,
                    )
                })
        });
        if let Some(target) = review {
            state.mark_agent_read(&target, false);
        }
        state.open_navigator_overlay();
        if let Some(ClientShellOverlay::Navigator(n)) = state.overlay.as_mut() {
            n.selected = Some(ClientNavigatorTarget::Pane {
                endpoint_id: ClientEndpointId::Local,
                pane_id: pane.into(),
            });
        }
        state.compose(size.0, size.1).expect("overview selection");
        if name != "overview status" {
            state.activate_overview(
                crate::client::shell::chrome::overview::Action::Preview,
                &mut ClientShellInput::default(),
            );
        }
        if reply {
            state.activate_overview(
                crate::client::shell::chrome::overview::Action::Reply,
                &mut ClientShellInput::default(),
            );
            state
                .insert_overview_text("请继续核对窄屏布局。\n补齐焦点和旧响应测试，不要代答权限。");
        }
        let frame = state
            .compose(size.0, size.1)
            .expect("overview frame")
            .frame
            .clone();
        shots.push(Shot {
            title: format!("{name} {}x{}", size.0, size.1),
            frame,
            host: if light_theme { LIGHT_HOST } else { DARK_HOST },
        });
    }
    write_shots("gallery", &shots);
}

/// 画廊用的主机快照：一台 32 线程工作站，两块 GPU、四块盘（根分区接近满）、三个接口、
/// 四个温度传感器与一屏多的进程——数字取真实机器上常见的量级。
fn gallery_system_sample(now_ms: u64) -> crate::api::schema::SystemMetricsSnapshot {
    use crate::api::schema::{
        CpuCoreMetric, DiskMetric, GpuMetric, MemoryMetric, NetworkMetric, ObservationStatus,
        ProcessIdentity, ProcessMetric, SensorMetric, SystemMetricsSnapshot,
    };
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    let cores = (0..32)
        .map(|id| {
            let usage = match id {
                3 | 17 => 97.0,
                5 | 21 => 81.0,
                _ => 12.0 + ((id * 37) % 53) as f32,
            };
            CpuCoreMetric {
                id,
                name: format!("cpu{id}"),
                usage_percent: Some(usage),
                frequency_mhz: Some(4_200),
            }
        })
        .collect();
    let process = |pid: u32, name: &str, user: &str, cpu: f32, memory: u64| ProcessMetric {
        identity: ProcessIdentity {
            pid,
            started_at: 1_700_000_000,
            boot_id: "gallery-boot".into(),
            instance_token: None,
        },
        parent_pid: Some(1),
        name: name.into(),
        cpu_percent: Some(cpu),
        memory_bytes: memory,
        status: "Running".into(),
        user: Some(user.into()),
        protected: false,
        action_token: Some(format!("token-{pid}")),
        executable: Some(format!("/usr/bin/{name}")),
    };
    let processes = vec![
        process(412_906, "rustc", "dev", 98.4, 1_900 * MIB),
        process(412_911, "rustc", "dev", 96.1, 1_420 * MIB),
        process(398_120, "cargo", "dev", 12.5, 310 * MIB),
        process(52_118, "claude", "dev", 7.8, 640 * MIB),
        process(52_377, "codex", "dev", 4.2, 380 * MIB),
        process(9_771, "herdr", "dev", 3.1, 96 * MIB),
        process(2_559, "Xorg", "root", 2.4, 210 * MIB),
        process(3_101, "firefox", "dev", 2.2, 2_870 * MIB),
        process(1_204, "postgres", "postgres", 1.1, 455 * MIB),
        process(1_377, "dockerd", "root", 0.9, 162 * MIB),
        process(4_012, "nvim", "dev", 0.6, 88 * MIB),
        process(4_090, "zsh", "dev", 0.1, 9 * MIB),
        process(812, "systemd-journald", "root", 0.1, 64 * MIB),
        process(1, "systemd", "root", 0.0, 14 * MIB),
    ]
    .into_iter()
    .chain((0..26u32).map(|index| {
        process(
            20_000 + index * 7,
            ["kworker/u64:2", "sshd", "node", "python3", "gnome-shell"][index as usize % 5],
            "dev",
            (index % 4) as f32 * 0.3,
            u64::from(12 + index * 9) * MIB,
        )
    }))
    .collect();
    SystemMetricsSnapshot {
        boot_id: "gallery-boot".into(),
        sequence: 900,
        sampled_at_ms: now_ms - 2_000,
        interval_ms: 1000,
        hostname: "devbox".into(),
        operating_system: "Ubuntu 24.04 LTS".into(),
        environment: "native".into(),
        uptime_seconds: 3 * 86_400 + 4 * 3_600 + 12 * 60,
        status: ObservationStatus::Ready,
        cpu_percent: Some(46.3),
        cpu_brand: "AMD Ryzen 9 7950X".into(),
        physical_core_count: Some(16),
        cores,
        memory: MemoryMetric {
            total_bytes: 64 * GIB,
            used_bytes: 41 * GIB,
            available_bytes: 17 * GIB,
            swap_total_bytes: 8 * GIB,
            swap_used_bytes: GIB + 200 * MIB,
        },
        gpus: vec![
            GpuMetric {
                id: "gpu0".into(),
                name: "NVIDIA GeForce RTX 4090".into(),
                status: ObservationStatus::Ready,
                usage_percent: Some(78.0),
                memory_used_bytes: Some(18 * GIB + 400 * MIB),
                memory_total_bytes: Some(24 * GIB),
                temperature_celsius: Some(71.0),
                ..Default::default()
            },
            GpuMetric {
                id: "gpu1".into(),
                name: "AMD Radeon Graphics".into(),
                status: ObservationStatus::Ready,
                usage_percent: Some(4.0),
                memory_used_bytes: Some(380 * MIB),
                memory_total_bytes: Some(2 * GIB),
                temperature_celsius: Some(48.0),
                ..Default::default()
            },
        ],
        disks: vec![
            DiskMetric {
                id: "nvme0n1p2:/".into(),
                name: "/dev/nvme0n1p2".into(),
                mount_point: "/".into(),
                total_bytes: 1_860 * GIB,
                available_bytes: 150 * GIB,
                ..Default::default()
            },
            DiskMetric {
                id: "nvme1n1p1:/data".into(),
                name: "/dev/nvme1n1p1".into(),
                mount_point: "/data".into(),
                total_bytes: 3_720 * GIB,
                available_bytes: 2_190 * GIB,
                ..Default::default()
            },
            DiskMetric {
                id: "sda1:/backup".into(),
                name: "/dev/sda1".into(),
                mount_point: "/backup".into(),
                total_bytes: 7_450 * GIB,
                available_bytes: 1_310 * GIB,
                ..Default::default()
            },
            DiskMetric {
                id: "nvme0n1p1:/boot/efi".into(),
                name: "/dev/nvme0n1p1".into(),
                mount_point: "/boot/efi".into(),
                total_bytes: GIB,
                available_bytes: GIB - 6 * MIB,
                ..Default::default()
            },
        ],
        networks: vec![
            NetworkMetric {
                id: "eth0".into(),
                received_bytes_per_second: Some(12.4 * 1_048_576.0),
                transmitted_bytes_per_second: Some(860.0 * 1024.0),
                ..Default::default()
            },
            NetworkMetric {
                id: "docker0".into(),
                received_bytes_per_second: Some(48.0 * 1024.0),
                transmitted_bytes_per_second: Some(31.0 * 1024.0),
                ..Default::default()
            },
            NetworkMetric {
                id: "wlan0".into(),
                received_bytes_per_second: Some(0.0),
                transmitted_bytes_per_second: Some(0.0),
                ..Default::default()
            },
        ],
        sensors: vec![
            SensorMetric {
                name: "k10temp Tctl".into(),
                temperature_celsius: Some(68.0),
                critical_celsius: Some(95.0),
            },
            SensorMetric {
                name: "k10temp Tccd1".into(),
                temperature_celsius: Some(61.0),
                critical_celsius: Some(95.0),
            },
            SensorMetric {
                name: "nvme Composite".into(),
                temperature_celsius: Some(44.0),
                critical_celsius: Some(84.0),
            },
            SensorMetric {
                name: "amdgpu edge".into(),
                temperature_celsius: Some(52.0),
                critical_celsius: Some(100.0),
            },
        ],
        processes,
        ..Default::default()
    }
}

/// 把一份画廊快照和 15 分钟的历史灌进监控状态，并按生产路径算好预分桶与进程行序。
fn seed_monitor(state: &mut ClientShellState) {
    use crate::client::shell::observability::HistoryPoint;
    const NOW_MS: u64 = 1_760_000_000_000;
    let monitor = &mut state.observability;
    monitor.now_ms = NOW_MS;
    monitor.metrics = Some(Box::new(gallery_system_sample(NOW_MS)));
    monitor.history = (0..900u64)
        .map(|second| {
            let t = second as f32;
            let wave = (t / 41.0).sin() * 14.0 + (t / 13.0).sin() * 6.0;
            let burst = if (520..640).contains(&second) {
                32.0
            } else {
                0.0
            };
            HistoryPoint {
                at: NOW_MS - 2_000 - (899 - second) * 1_000,
                cpu: Some((34.0 + wave + burst).clamp(2.0, 99.0)),
                memory: Some(58.0 + t / 900.0 * 6.0 + (t / 70.0).sin()),
                cores: Vec::new(),
                temperature: Some(58.0 + wave * 0.4 + burst * 0.3),
            }
        })
        .collect();
    for (id, rx, tx) in [
        ("eth0", 12.4 * 1_048_576.0_f32, 860.0 * 1024.0_f32),
        ("docker0", 48.0 * 1024.0, 31.0 * 1024.0),
    ] {
        let samples = (0..128)
            .map(|index| {
                let t = index as f32;
                let swing = 0.55 + 0.45 * (t / 9.0).sin().abs();
                (rx * swing, tx * (1.2 - swing * 0.6))
            })
            .collect();
        monitor.net_history.insert(id.into(), samples);
    }
    monitor.refresh_history_buckets();
    monitor.refresh_process_rows();
}

/// 监控面板画廊：停靠在终端旁（200×50，面板聚焦与终端聚焦各一帧）、聚焦并最大化
/// （160×45）、偏好页、窄屏 80×24、尚未连上采样器的空状态与 graphite-light。
/// `python scripts/ui_gallery.py --filter monitor`。
#[test]
#[ignore = "manual visual gallery; run `just ui-gallery`"]
fn ui_gallery_monitor() {
    use crate::client::shell::dock::PanelId;
    use crate::client::shell::observability::Page;
    let _lang = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    let dark = || ClientShellConfig::from_config(&Config::default());
    let light = || {
        let mut config = Config::default();
        config.theme.name = Some("graphite-light".into());
        ClientShellConfig::from_config(&config)
    };
    let docked: Prepare = |state| {
        state.open_observation_page(Page::Monitor, &mut ClientShellInput::default());
        seed_monitor(state);
    };
    let terminal_focused: Prepare = |state| {
        state.open_observation_page(Page::Monitor, &mut ClientShellInput::default());
        seed_monitor(state);
        if let Some(group) = state.workbench.dock.groups.first() {
            state.workbench.dock.focused = PanelId::Terminal(group.id);
        }
        state.observability.page = None;
    };
    let maximized: Prepare = |state| {
        state.open_observation_page(Page::Monitor, &mut ClientShellInput::default());
        seed_monitor(state);
        state.workbench.dock.maximized = Some(PanelId::Monitor);
    };
    let preferences: Prepare = |state| {
        state.open_observation_page(Page::Settings, &mut ClientShellInput::default());
        seed_monitor(state);
        state.observability.monitor.alerts_enabled = true;
        state.workbench.dock.maximized = Some(PanelId::Monitor);
    };
    let connecting: Prepare = |state| {
        state.open_observation_page(Page::Monitor, &mut ClientShellInput::default());
    };
    fn process_dialog_scene(state: &mut ClientShellState) {
        use crate::client::shell::observability::{Action, ProcessDialog};
        state.open_observation_page(Page::Monitor, &mut ClientShellInput::default());
        seed_monitor(state);
        let process = state
            .observability
            .metrics
            .as_deref()
            .and_then(|sample| sample.processes.first())
            .cloned()
            .expect("gallery process");
        state.observability.process_dialog = Some(ProcessDialog {
            process,
            force: false,
            confirm: false,
            pending: false,
        });
        state.observability.hovered_action = Some(Action::Terminate(true));
    }
    let confirmation: Prepare = |state| {
        process_dialog_scene(state);
        if let Some(dialog) = state.observability.process_dialog.as_mut() {
            dialog.force = true;
            dialog.confirm = true;
        }
        state.observability.hovered_action =
            Some(crate::client::shell::observability::Action::ConfirmProcess);
    };
    let hovered: Prepare = |state| {
        state.open_observation_page(Page::Monitor, &mut ClientShellInput::default());
        seed_monitor(state);
        state.observability.hovered_action = Some(
            crate::client::shell::observability::Action::Page(Page::Settings),
        );
    };
    let scenes: [(&str, (u16, u16), Prepare, bool); 11] = [
        ("monitor system docked", (200, 50), docked, false),
        (
            "monitor terminal focused",
            (200, 50),
            terminal_focused,
            false,
        ),
        ("monitor system maximized", (160, 45), maximized, false),
        ("monitor preferences", (160, 45), preferences, false),
        ("monitor narrow", (80, 24), docked, false),
        ("monitor connecting", (120, 40), connecting, false),
        ("monitor graphite-light", (200, 50), docked, true),
        ("monitor hover", (160, 45), hovered, false),
        (
            "monitor process narrow",
            (60, 24),
            process_dialog_scene,
            false,
        ),
        (
            "monitor process light",
            (60, 24),
            process_dialog_scene,
            true,
        ),
        (
            "monitor process confirmation",
            (160, 45),
            confirmation,
            false,
        ),
    ];
    let mut shots = Vec::new();
    let mut step = 0;
    for (name, size, prepare, is_light) in scenes {
        let config = if is_light { light() } else { dark() };
        let frame = shoot_scene(config, size, prepare, &mut step);
        let title = format!("{name} {}x{}", size.0, size.1);
        shots.push(if is_light {
            Shot {
                title,
                frame,
                host: LIGHT_HOST,
            }
        } else {
            Shot::dark(title, frame)
        });
    }
    write_shots("monitor", &shots);
}

/// 子智能体页面画廊的节点：Codex 风格的子智能体（昵称 · 任务），时间相对 `now_ms`。
fn agent_page_nodes(now_ms: u64) -> Vec<crate::api::schema::AgentActivityNode> {
    let node = |id: &str,
                parent: Option<&str>,
                label: &str,
                role: &str,
                status: AgentActivityStatus,
                (start, end): (u64, Option<u64>),
                summary: &str| crate::api::schema::AgentActivityNode {
        id: id.into(),
        kind: AgentActivityKind::Subagent,
        label: label.into(),
        status,
        parent_id: parent.map(str::to_owned),
        agent_type: Some(role.into()),
        content_ref: Some(format!("ref-{id}")),
        summary: Some(summary.into()),
        started_at_ms: Some(now_ms.saturating_sub(start)),
        ended_at_ms: end.map(|end| now_ms.saturating_sub(end)),
    };
    vec![
        node(
            "boyle",
            None,
            "Boyle · repo_scan",
            "explorer",
            AgentActivityStatus::Done,
            (360_000, Some(252_000)),
            "定位 14 个界面入口，汇总到 surfaces.md",
        ),
        node(
            "hume",
            None,
            "Hume · ui_audit",
            "worker",
            AgentActivityStatus::Running,
            (252_000, None),
            "exec  just ui-gallery --filter agent",
        ),
        node(
            "ada",
            Some("hume"),
            "Ada · screenshot",
            "worker",
            AgentActivityStatus::Running,
            (41_000, None),
            "exec  msedge --headless --screenshot target/tmp/ui-audit/page.png",
        ),
        node(
            "noether",
            None,
            "Noether · codex_hooks",
            "explorer",
            AgentActivityStatus::Done,
            (330_000, Some(148_000)),
            "核对钩子事件与载荷字段，写入 hooks.md",
        ),
        node(
            "turing",
            None,
            "Turing · fix_tests",
            "worker",
            AgentActivityStatus::Failed,
            (300_000, Some(160_000)),
            "cargo nextest 失败：3 个测试未通过",
        ),
        node(
            "guardian",
            None,
            "guardian",
            "审核",
            AgentActivityStatus::Done,
            (60_000, Some(51_000)),
            "apply_patch 已批准 · src/ui/kit/card.rs",
        ),
    ]
}

/// 子智能体页面画廊的转写（Codex Text 格式）。
const AGENT_PAGE_TRANSCRIPT: &str = "[message from /root → /root/ui_audit] 审计 herdr 全部 TUI 界面，按严重度输出问题清单和可直接实施的修改建议；截图写到临时目录，不要改仓库。
(thinking)
[exec_command] rg --files src/client/shell | rg overlay
    src/client/shell/agent_activity_overlay.rs
    src/client/shell/machine_files_overlay.rs
[exec_command] rg -n \"ModalSize::Large\" src/client/shell
    agent_activity_overlay.rs:1085    .or_else(|| crate::ui::modal_rect(area, crate::ui::ModalSize::Large))
    machine_files_overlay.rs:1003    .or_else(|| crate::ui::modal_rect(area, ModalSize::Large))
    … +6 lines
活动窗口固定 76×22，在 200×50 终端里只占约 17% 面积；树列只有 25 列，Codex 子智能体名称几乎都被截断。下一步核对三处活动树的状态色。
[spawn_agent] screenshot · worker · gpt-6.1-sol
[wait_agent] → Ada
";

/// 把页面在途的 `agent.activity.read` 逐个应答：树读取回 `nodes`，内容读取回转写。
/// `limit` 限制应答次数（0 = 不应答，看加载中）。
fn answer_agent_page(
    state: &mut ClientShellState,
    nodes: &[crate::api::schema::AgentActivityNode],
    limit: usize,
) {
    for _ in 0..limit {
        let Some((request_id, boot_id, node_id)) =
            state
                .pending_requests
                .iter()
                .find_map(|(id, pending)| match &pending.kind {
                    PendingEndpointKind::AgentActivityRead { node_id, .. } => {
                        Some((id.clone(), pending.boot_id.clone(), node_id.clone()))
                    }
                    _ => None,
                })
        else {
            return;
        };
        let result = match node_id {
            None => crate::api::schema::ResponseResult::AgentActivity {
                nodes: nodes.to_vec(),
                content: None,
            },
            Some(node) => crate::api::schema::ResponseResult::AgentActivity {
                nodes: Vec::new(),
                content: Some(crate::api::schema::AgentActivityContent {
                    node_id: node,
                    format: crate::api::schema::AgentActivityContentFormat::Text,
                    text: AGENT_PAGE_TRANSCRIPT.into(),
                    eof: true,
                    truncated: false,
                    next_cursor: Some("end".into()),
                }),
            },
        };
        state.handle_endpoint_result(&boot_id, &request_id, Ok(result));
    }
}

/// 子智能体页面的一个场景：在工作台里打开 codex（p3，改成运行中）的页面，按需应答
/// 读取、选中节点，再交给 `prepare` 调整后合成。
struct AgentPageScene {
    name: &'static str,
    owner: super::super::agent_activity_overlay::AgentActivityOwner,
    nodes: bool,
    answers: usize,
    select: Option<&'static str>,
    supported: bool,
    prepare: Prepare,
}

fn shoot_agent_page(
    config: ClientShellConfig,
    (cols, rows): (u16, u16),
    scene: &AgentPageScene,
    step: &mut u64,
) -> FrameData {
    let mut state = workbench_state(config);
    let mut snapshot = gallery_snapshot();
    snapshot.revision += 1;
    if let Some(codex) = snapshot
        .agents
        .iter_mut()
        .find(|agent| agent.pane_id == "p3")
    {
        codex.agent_status = AgentStatus::Working;
        codex.insight.model = Some("gpt-6.1-sol high".into());
        codex.insight.session_title = Some("重构 Agent 活动窗口并补齐 Codex 适配".into());
        codex.insight.question = None;
        codex.insight.options.clear();
        codex.insight.working_since_ms = Some(
            crate::harness::time::system_time_ms(std::time::SystemTime::now())
                .unwrap_or_default()
                .saturating_sub(372_000),
        );
    }
    state.set_endpoint_snapshot_for_generation(
        &ClientEndpointId::Local,
        GENERATION,
        Box::new(snapshot),
    );
    if !scene.supported {
        state.set_endpoint_methods(Some(vec!["client.views.set".into(), "tab.focus".into()]));
    }
    let _ = shoot_workbench(&mut state, cols, rows, step);
    let now_ms =
        crate::harness::time::system_time_ms(std::time::SystemTime::now()).unwrap_or_default();
    let nodes = if scene.nodes {
        agent_page_nodes(now_ms)
    } else {
        Vec::new()
    };
    state.open_agent_activity(
        ClientEndpointId::Local,
        scene.owner.clone(),
        &mut ClientShellInput::default(),
    );
    answer_agent_page(&mut state, &nodes, scene.answers.min(1));
    if let Some(node) = scene.select {
        state.select_agent_activity_node(node.into(), &mut ClientShellInput::default());
    }
    answer_agent_page(&mut state, &nodes, scene.answers.saturating_sub(1));
    // 先合成一帧：转写在视图计算阶段同步，之后的按键（如 Enter 全部展开）才有区块可切。
    let _ = state.compose(cols, rows);
    (scene.prepare)(&mut state);
    // 两次合成：第一次建列表显示行并把选中项滚进视野。
    let _ = state.compose(cols, rows);
    shoot_workbench(&mut state, cols, rows, step)
}

#[test]
#[ignore = "manual visual gallery; run `just ui-gallery`"]
fn ui_gallery_agent_page() {
    use super::super::agent_activity_overlay::AgentActivityOwner;
    let _lang = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    let dark = || ClientShellConfig::from_config(&Config::default());
    let light = || {
        let mut config = Config::default();
        config.theme.name = Some("graphite-light".into());
        ClientShellConfig::from_config(&config)
    };
    let pane = || AgentActivityOwner::Pane {
        pane_id: "p3".into(),
    };
    let scene = |name, select, prepare| AgentPageScene {
        name,
        owner: pane(),
        nodes: true,
        answers: 4,
        select,
        supported: true,
        prepare,
    };
    let none: Prepare = |_| {};
    let status: Prepare = |state| {
        state.handle_raw_events(vec![RawInputEvent::Key(crate::input::TerminalKey::new(
            KeyCode::Char('2'),
            KeyModifiers::NONE,
        ))]);
    };
    let timeline: Prepare = |state| {
        state.handle_raw_events(vec![RawInputEvent::Key(crate::input::TerminalKey::new(
            KeyCode::Char('3'),
            KeyModifiers::NONE,
        ))]);
    };
    let filtering: Prepare = |state| {
        let keys = ['/', 'h', 'u'];
        state.handle_raw_events(
            keys.iter()
                .map(|ch| {
                    RawInputEvent::Key(crate::input::TerminalKey::new(
                        KeyCode::Char(*ch),
                        KeyModifiers::NONE,
                    ))
                })
                .collect(),
        );
    };
    let transcript: Prepare = |state| {
        state.handle_raw_events(vec![RawInputEvent::Key(crate::input::TerminalKey::new(
            KeyCode::Tab,
            KeyModifiers::NONE,
        ))]);
    };
    let expanded: Prepare = |state| {
        state.handle_raw_events(
            [KeyCode::Tab, KeyCode::Enter]
                .into_iter()
                .map(|code| {
                    RawInputEvent::Key(crate::input::TerminalKey::new(code, KeyModifiers::NONE))
                })
                .collect(),
        );
    };
    let mut scenes = Vec::new();
    // 窄屏单栏：焦点切到转写，看转写在窄宽度下的折行与「+N 行」。
    for size in [(80, 24), (60, 20)] {
        scenes.push((scene("transcript", Some("hume"), transcript), size));
    }
    // 转写全部展开：工具输出的边线对齐与区块间距。
    for size in [(160, 45), (80, 24)] {
        scenes.push((scene("expanded", Some("hume"), expanded), size));
    }
    for size in [(200, 50), (160, 45), (120, 40), (80, 24), (60, 20)] {
        scenes.push((scene("tree", Some("hume"), none), size));
    }
    for (name, select, prepare) in [
        ("status", Some("hume"), status),
        ("timeline", Some("hume"), timeline),
        ("failed", Some("turing"), none),
        ("filter", Some("hume"), filtering),
    ] {
        scenes.push((scene(name, select, prepare), (160, 45)));
    }
    scenes.push((
        AgentPageScene {
            name: "external",
            owner: AgentActivityOwner::External {
                external_id: "zcode:01".into(),
            },
            ..scene("external", Some("boyle"), none)
        },
        (160, 45),
    ));
    scenes.push((
        AgentPageScene {
            supported: false,
            answers: 0,
            ..scene("unsupported", None, none)
        },
        (160, 45),
    ));
    scenes.push((
        AgentPageScene {
            answers: 0,
            ..scene("loading", None, none)
        },
        (160, 45),
    ));
    scenes.push((
        AgentPageScene {
            nodes: false,
            ..scene("empty", None, none)
        },
        (160, 45),
    ));
    scenes.push((scene("timeline", Some("turing"), timeline), (120, 40)));
    scenes.push((scene("timeline", Some("hume"), timeline), (80, 24)));
    let mut shots = Vec::new();
    let mut step = 0;
    for (scene, size) in &scenes {
        for (theme, host) in [("dark", DARK_HOST), ("light", LIGHT_HOST)] {
            let config = if theme == "light" { light() } else { dark() };
            let frame = shoot_agent_page(config, *size, scene, &mut step);
            shots.push(Shot {
                title: format!("agent page {} {theme} {}x{}", scene.name, size.0, size.1),
                frame,
                host,
            });
        }
    }
    // 经典布局：页面占满整帧，最后一行是页面自己的页脚。
    let mut state = classic_state(dark());
    state.set_endpoint_methods(Some(vec!["agent.activity.read".into()]));
    let _ = shoot_classic(&mut state, 140, 40);
    let nodes = agent_page_nodes(
        crate::harness::time::system_time_ms(std::time::SystemTime::now()).unwrap_or_default(),
    );
    state.open_agent_activity(
        ClientEndpointId::Local,
        pane(),
        &mut ClientShellInput::default(),
    );
    answer_agent_page(&mut state, &nodes, 1);
    state.select_agent_activity_node("hume".into(), &mut ClientShellInput::default());
    answer_agent_page(&mut state, &nodes, 3);
    let _ = state.compose(140, 40);
    shots.push(Shot::dark(
        "agent page classic dark 140x40".into(),
        state.compose(140, 40).expect("经典布局页面").frame.clone(),
    ));
    write_shots("agent-page", &shots);
}

#[path = "gallery_tools.rs"]
mod tools;

#[path = "gallery_toast.rs"]
mod toast;
