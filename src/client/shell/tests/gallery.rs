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
        insight: Default::default(),
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

    write_shots("gallery", &shots);
}
