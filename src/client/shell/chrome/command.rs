use super::super::action_table::ActionId;
use super::super::command_palette::{ClientPaletteAction, ClientPaletteItem, ClientPaletteRow};
use super::super::*;
use crate::ui::display_width_u16;

pub(in crate::client::shell) fn text(en: &'static str, zh: &'static str) -> &'static str {
    if crate::i18n::lang() == crate::i18n::Lang::ZhCn {
        zh
    } else {
        en
    }
}

pub(in crate::client::shell) fn description(id: ActionId) -> &'static str {
    use ActionId::*;
    let (en, zh) = match id {
        WhatsNew => (
            "Read release notes and update instructions",
            "阅读发行说明与更新步骤",
        ),
        Settings => (
            "Language, theme, notifications and integrations",
            "语言、主题、通知与集成",
        ),
        Help => (
            "Search your current keyboard bindings",
            "搜索当前配置的快捷键",
        ),
        Notifications => (
            "Review recent messages and jump to their sessions",
            "查看最近消息并跳转到来源会话",
        ),
        ReloadConfig => (
            "Reload local presentation and the selected server",
            "重载本地界面与所选服务器配置",
        ),
        ManageMachines | MachineImport => (
            "Manage local and saved SSH connections",
            "管理本机与已保存的 SSH 连接",
        ),
        WorkspacePicker | OpenNavigator => {
            ("Find and focus an existing session", "查找并聚焦已有会话")
        }
        NextAttention => (
            "Jump to the next session needing attention",
            "跳转到下一个需要关注的会话",
        ),
        ToggleSessionGrouping => (
            "Group sessions by workspace or agent status",
            "按工作区或智能体状态分组会话",
        ),
        MarkUnread => ("Mark this session as unread", "将此会话标为未读"),
        MarkAllRead => ("Mark all sessions as read", "将全部会话标为已读"),
        ToggleInspector => (
            "Show or hide read-only session details",
            "显示或隐藏只读会话详情",
        ),
        NewWorkspace => ("Start a shell in a new workspace", "在新工作区启动 shell"),
        NewWorktree => ("Create an isolated Git checkout", "创建独立的 Git 检出目录"),
        OpenWorktree => ("Open an existing Git checkout", "打开已有 Git 检出目录"),
        RemoveWorktree => (
            "Remove the checkout folder, keep the branch",
            "删除检出目录，保留分支",
        ),
        RenameWorkspace | RenameTab | RenamePane | AgentRename | MachineRename => (
            "Edit the display name without changing identity",
            "修改显示名称，不改变对象身份",
        ),
        CloseWorkspace => (
            "Close this workspace and its running terminals",
            "关闭此工作区及正在运行的终端",
        ),
        CloseTab => (
            "Close the tab and every terminal inside it",
            "关闭标签页及其全部终端",
        ),
        ClosePane | AgentClose => ("Stop and close the target terminal", "停止并关闭目标终端"),
        ToggleWorkspaceGroup => (
            "Expand or collapse linked checkouts",
            "展开或收起关联工作树",
        ),
        PreviousWorkspace | NextWorkspace => (
            "Switch workspace without stopping running agents",
            "切换工作区，不停止运行中的智能体",
        ),
        PreviousAgent | NextAgent | AgentFocus => {
            ("Jump to the agent's terminal", "跳转到智能体所在终端")
        }
        AgentViewActivity => (
            "Read the agent's tools, tasks and background work",
            "查看智能体工具、任务与后台活动",
        ),
        NewTab => (
            "Start another terminal in this workspace",
            "在当前工作区启动另一终端",
        ),
        PreviousTab | NextTab => (
            "Switch the visible terminal tab",
            "切换当前显示的终端标签页",
        ),
        MoveTabPrevious | MoveTabNext => (
            "Reorder this tab within its workspace",
            "调整此标签页在工作区内的顺序",
        ),
        CopyPaneSelection => ("Copy the selected terminal text", "复制已选中的终端文本"),
        ClearPaneName => ("Restore the automatic terminal name", "恢复终端的自动名称"),
        SwapWithFocusedPane => (
            "Exchange positions with the focused pane",
            "与聚焦窗格交换位置",
        ),
        EditScrollback => (
            "Open terminal history in your editor",
            "在编辑器中打开终端历史",
        ),
        CopyMode => (
            "Search and select terminal scrollback",
            "搜索并选择终端历史文本",
        ),
        SplitRight | SplitDown => (
            "Split this terminal and start another shell",
            "拆分当前终端并启动另一 shell",
        ),
        Zoom => (
            "Show only the focused pane or restore the split",
            "仅显示聚焦窗格或恢复拆分布局",
        ),
        RightClickPassthrough => (
            "Send right-clicks to the program in this pane",
            "将右键点击交给窗格内的程序",
        ),
        EnterResizeMode => (
            "Resize pane splits with directional keys",
            "使用方向键调整窗格分割比例",
        ),
        FocusPaneLeft | FocusPaneDown | FocusPaneUp | FocusPaneRight | CyclePaneNext
        | CyclePanePrevious | LastPane => (
            "Move terminal focus without changing the layout",
            "移动终端焦点，不改变布局",
        ),
        LinkHints => (
            "Open a terminal link using keyboard hints",
            "通过键位提示打开终端链接",
        ),
        ToggleSidebar => ("Show or hide the session sidebar", "显示或隐藏会话侧栏"),
        Detach => (
            "Leave the client; keep server sessions running",
            "退出客户端，保留服务器会话",
        ),
        CustomCommand => ("Run the configured command", "执行配置中的命令"),
        Snippets | SnippetRun => ("Select and send a saved snippet", "选择并发送已保存片段"),
        SceneSave | SceneRestore => (
            "Save or restore workspace arrangements",
            "保存或恢复工作区场景",
        ),
        Broadcast => (
            "Send input to explicitly selected terminals",
            "向明确选中的终端广播输入",
        ),
        MachineConnect => (
            "Connect or reconnect to this SSH machine",
            "连接或重新连接此 SSH 机器",
        ),
        MachineSwitch => ("Make this connected machine active", "切换到此已连接机器"),
        MachineEdit => (
            "Edit SSH host, user and connection options",
            "编辑 SSH 地址、用户与连接选项",
        ),
        MachineEnabled => (
            "Enable or disable this saved connection",
            "启用或禁用此已保存连接",
        ),
        MachineCopyFixCommand => (
            "Copy the command for repairing remote setup",
            "复制远端环境修复命令",
        ),
        MachineRemove => (
            "Remove the saved connection, not remote files",
            "移除已保存连接，不删除远端文件",
        ),
        Monitor => (
            "Open live CPU, memory, GPU and process charts",
            "打开 CPU、内存、GPU 与进程实时图表",
        ),
        CloseMonitor => ("Hide the monitor panel", "隐藏监控面板"),
        ArrangeLayout => (
            "Drag panels and terminal groups into place",
            "拖动面板与终端组调整位置",
        ),
        LockLayout => (
            "Prevent panel dragging and resizing",
            "阻止面板拖动与尺寸调整",
        ),
        ResetLayout => ("Restore the default panel arrangement", "恢复默认面板排列"),
    };
    text(en, zh)
}

pub(in crate::client::shell) fn category_description(category: usize) -> &'static str {
    match category {
        0 => text(
            "Create, switch and manage Git checkouts",
            "新建、切换与管理 Git 检出",
        ),
        1 => text("Split, focus and arrange terminals", "拆分、聚焦与排列终端"),
        2 => text("Local and SSH connection actions", "本机与 SSH 连接操作"),
        3 => text(
            "Live host resources and monitor preferences",
            "实时主机资源与监控偏好",
        ),
        4 => text(
            "Snippets, scenes and configured commands",
            "片段、场景与配置命令",
        ),
        5 => text(
            "Theme, language and agent integrations",
            "主题、语言与智能体集成",
        ),
        _ => text(
            "Shortcuts, notifications and release notes",
            "快捷键、通知与发行说明",
        ),
    }
}

pub(in crate::client::shell) fn item_description(item: &ClientPaletteItem) -> &str {
    match &item.action {
        ClientPaletteAction::Workspace(..) | ClientPaletteAction::Agent(..) => &item.subtitle,
        ClientPaletteAction::Run(
            ActionId::CustomCommand,
            super::super::action_table::ActionTarget::Command(command),
        ) => &command.command,
        ClientPaletteAction::Run(id, _) => description(*id),
        ClientPaletteAction::Category(category) => category_description(*category),
        ClientPaletteAction::Search => text(
            "Find a command, workspace or agent",
            "查找命令、工作区或智能体",
        ),
        ClientPaletteAction::Back => text("Return to the main menu", "返回主菜单"),
    }
}

pub(in crate::client::shell) fn section(row: &ClientPaletteRow<'_>) -> u8 {
    if row.recent {
        0
    } else {
        match row.item.action {
            ClientPaletteAction::Agent(..) => 1,
            ClientPaletteAction::Workspace(..) => 2,
            _ => 3,
        }
    }
}

pub(in crate::client::shell) fn visual(
    rows: &[ClientPaletteRow<'_>],
) -> Vec<(Option<usize>, &'static str)> {
    let mut out = Vec::new();
    let mut previous = None;
    for (index, row) in rows.iter().enumerate() {
        let group = section(row);
        if previous != Some(group) {
            let label = match group {
                0 => text("Recent", "最近"),
                1 => text("Agents", "智能体"),
                2 => text("Workspaces", "工作区"),
                _ => text("Commands", "命令"),
            };
            out.push((None, label));
            previous = Some(group);
        }
        out.push((Some(index), ""));
    }
    out
}

pub(in crate::client::shell) const SIZE: crate::ui::ModalSize = crate::ui::ModalSize::Content {
    width: 104,
    height: 28,
};

pub(in crate::client::shell) fn paint_row(
    buffer: &mut Buffer,
    rect: Rect,
    row: &ClientPaletteRow<'_>,
    selected: bool,
    hovered: bool,
    cx: &super::super::feedback::ChromeContext<'_>,
    status_indicators: crate::config::StatusIndicatorStyle,
) {
    let palette = cx.palette;
    let components = cx.components;
    let bg = if selected {
        super::settings::selected_bg(palette)
    } else if hovered {
        palette.surface_dim
    } else {
        palette.panel_bg
    };
    let base = Style::default()
        .fg(palette.text)
        .bg(bg)
        .remove_modifier(Modifier::all());
    buffer.set_style(rect, base);
    let title_style = if selected {
        base.add_modifier(Modifier::BOLD)
    } else {
        base
    };
    let danger = matches!(row.item.action, ClientPaletteAction::Run(id, _) if super::super::action_table::action_spec(id).danger);
    let title_style = if danger {
        title_style.fg(palette.red)
    } else if !row.match_indices.is_empty() {
        title_style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else {
        title_style
    };
    let glyph = match row.item.action {
        ClientPaletteAction::Agent(_, _, bucket, ..) => {
            crate::ui::design::status_glyph(bucket, status_indicators, 0, false)
        }
        ClientPaletteAction::Workspace(..) => "▣",
        _ if row.item.checked == Some(true) => "✓",
        _ if danger => "✕",
        _ => "·",
    };
    let glyph_style = if let ClientPaletteAction::Agent(_, _, bucket, ..) = row.item.action {
        title_style.fg(crate::ui::design::status_color(bucket, palette, components))
    } else {
        title_style
    };
    super::super::render::put_text(buffer, rect.x + 1, rect.y, 1, glyph, glyph_style);
    let key = if matches!(row.item.action, ClientPaletteAction::Run(..))
        && !row.item.subtitle.is_empty()
        && row.item.group < 0x100
    {
        Some(row.item.subtitle.as_str())
    } else {
        None
    };
    let key_width = key.map_or(0, |key| display_width_u16(key).saturating_add(2));
    let key_width = if key_width + 14 < rect.width {
        key_width
    } else {
        0
    };
    let badge_width = if row.item.badge { 2 } else { 0 };
    let available = rect.width.saturating_sub(4 + key_width + badge_width);
    let title_width = if available >= 52 {
        (available / 2).min(36)
    } else {
        available
    };
    super::super::render::put_text(
        buffer,
        rect.x + 3,
        rect.y,
        title_width,
        &super::settings::truncate_with_ellipsis(&row.item.title, title_width),
        title_style,
    );
    if available > title_width + 8 {
        let room = available - title_width - 2;
        super::super::render::put_text(
            buffer,
            rect.x + 3 + title_width + 2,
            rect.y,
            room,
            &super::settings::truncate_with_ellipsis(item_description(row.item), room),
            base.fg(palette.subtext0),
        );
    }
    if row.item.badge {
        super::super::render::put_text(
            buffer,
            rect.right() - key_width - 1,
            rect.y,
            1,
            "●",
            base.fg(palette.accent),
        );
    }
    if let Some(key) = key.filter(|_| key_width > 0) {
        super::super::render::put_text(
            buffer,
            rect.right() - key_width,
            rect.y,
            key_width,
            &format!(" {key} "),
            super::bar_key_cap(palette, bg),
        );
    }
}
