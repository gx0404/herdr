//! 统一动作表：命令面板（全集）、主菜单（按分类）、右键菜单（按对象）与 mobile
//! 菜单共用的动作目录。
//!
//! 每个动作只在 [`ACTIONS`] 里定义一次：稳定 id（最近使用持久化用）、标题与
//! 菜单短标签的 i18n 键、分类与组、快捷键来源、出现在哪些入口、在右键菜单里的
//! 身份。可用性由本模块的纯函数判定（[`global_action_state`]、
//! [`machine_action_state`]），执行只走 [`ClientShellState::run_action`] 这一个
//! 分派。同一个动作可以作用在「当前聚焦对象」（快捷键语义：命令面板、主菜单）
//! 或「右键点中的对象」上，由 [`ActionTarget`] 区分，条目本身只有一份。
//!
//! 机器动作（重连、切换、重命名、编辑、启用、复制修复命令、移除）是同一组条目：
//! 命令面板按机器铺开，右键机器行按对象取，执行都落到同一个分支。

use super::*;
use crate::config::{ActionKeybinds, CustomCommandKeybind, Keybinds};
use crate::i18n::Texts;
use crate::input::KeybindAction;

/// 主菜单分类，下标即 `GlobalMenuTexts::categories` 的位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ActionCategory {
    Workspaces = 0,
    TabsPanes = 1,
    Machines = 2,
    Monitor = 3,
    Tools = 4,
    Settings = 5,
    Help = 6,
}

impl ActionCategory {
    pub(super) const fn index(self) -> usize {
        self as usize
    }
}

/// 动作的唯一身份。新增变体必须同时在 [`ACTIONS`] 里登记一条并在
/// [`ClientShellState::run_action`] 里给出执行分支（后者是穷尽 match，漏了编译
/// 不过；前者由单测守门）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum ActionId {
    WhatsNew,
    Settings,
    ManageMachines,
    Notifications,
    Help,
    ReloadConfig,
    WorkspacePicker,
    OpenNavigator,
    NewWorkspace,
    NewWorktree,
    OpenWorktree,
    RemoveWorktree,
    RenameWorkspace,
    CloseWorkspace,
    ToggleWorkspaceGroup,
    PreviousWorkspace,
    NextWorkspace,
    PreviousAgent,
    NextAgent,
    NewTab,
    RenameTab,
    PreviousTab,
    NextTab,
    MoveTabPrevious,
    MoveTabNext,
    CloseTab,
    RenamePane,
    ClearPaneName,
    SwapWithFocusedPane,
    EditScrollback,
    CopyMode,
    SplitRight,
    SplitDown,
    ClosePane,
    Zoom,
    RightClickPassthrough,
    EnterResizeMode,
    FocusPaneLeft,
    FocusPaneDown,
    FocusPaneUp,
    FocusPaneRight,
    CyclePaneNext,
    CyclePanePrevious,
    LastPane,
    LinkHints,
    ToggleSidebar,
    Detach,
    CustomCommand,
    MachineImport,
    Snippets,
    SnippetRun,
    SceneSave,
    SceneRestore,
    Broadcast,
    MachineConnect,
    MachineSwitch,
    MachineRename,
    MachineEdit,
    MachineEnabled,
    MachineCopyFixCommand,
    MachineRemove,
    Monitor,
    CloseMonitor,
    ArrangeLayout,
    LockLayout,
    AgentFocus,
    AgentViewActivity,
    AgentRename,
    AgentUsage,
    AgentBindAccount,
    AgentClose,
}

/// 动作在命令面板 / 主菜单里的出现方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PaletteMode {
    /// 不进命令面板（只在右键菜单里出现）。
    Hidden,
    /// 一条，作用在当前聚焦对象上。
    Focused,
    /// 每台已保存的 SSH 机器一条，标题是带 `{label}` 的模板。
    PerMachine,
    /// 配置里每条自定义命令一条。
    PerCommand,
}

/// 右键菜单的对象种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ContextKind {
    Workspace,
    Tab,
    Pane,
    Machine,
    Agent,
}

/// 快捷键来源：按下它触发的 [`KeybindAction`] 与读取用户键位的访问器。
#[derive(Clone, Copy)]
pub(super) struct ActionBinding {
    pub(super) action: KeybindAction,
    pub(super) keys: fn(&Keybinds) -> &ActionKeybinds,
}

type TextKey = fn(&Texts) -> &'static str;

/// 动作表的一条。
pub(super) struct ActionSpec {
    pub(super) id: ActionId,
    /// 稳定 id：命令面板「最近使用」持久化与测试定位都认它，改名即丢失用户的
    /// 最近记录。按机器 / 按命令铺开的条目在后面拼 `:<profile>` / `:<command>`。
    pub(super) key: &'static str,
    /// 命令面板标题（按机器铺开的是带 `{label}` 的模板）。
    pub(super) title: TextKey,
    /// 状态为 `alternate` 时换用的标题（如「更新内容」→「更新就绪」）。
    pub(super) alt_title: Option<TextKey>,
    /// 右键菜单 / 主菜单子菜单里的短标签；`None` 用 `title`。
    pub(super) label: Option<TextKey>,
    /// 状态为 `alternate` 时换用的短标签（如「关闭」→「关闭分组」）。
    pub(super) alt_label: Option<TextKey>,
    pub(super) category: ActionCategory,
    /// 分类内的组号：主菜单子菜单在组号变化处画分隔线。
    // 主菜单目录视图接 kit::menu 的提交里读取，届时删除本 allow。
    #[allow(dead_code)]
    pub(super) group: u8,
    pub(super) binding: Option<ActionBinding>,
    pub(super) palette: PaletteMode,
    /// 出现在 mobile 菜单里。
    pub(super) mobile: bool,
    /// 在右键菜单里的身份：对象种类 + 该对象菜单里的动作。
    pub(super) context: Option<(ContextKind, ClientContextMenuAction)>,
    /// 破坏性动作（关闭、移除）：菜单里标红。
    // 右键菜单接 kit::menu 的提交里读取，届时删除本 allow。
    #[allow(dead_code)]
    pub(super) danger: bool,
}

impl ActionSpec {
    pub(super) fn title_text(&self, texts: &Texts, alternate: bool) -> &'static str {
        match self.alt_title.filter(|_| alternate) {
            Some(alt) => alt(texts),
            None => (self.title)(texts),
        }
    }

    pub(super) fn label_text(&self, texts: &Texts, alternate: bool) -> &'static str {
        match (self.alt_label.filter(|_| alternate), self.label) {
            (Some(alt), _) => alt(texts),
            (None, Some(label)) => label(texts),
            (None, None) => self.title_text(texts, alternate),
        }
    }

    /// 用户当前键位的首个标签（菜单右侧只放一个，多个键位时取第一个）。
    // 右键菜单接 kit::menu 的提交里读取，届时删除本 allow。
    #[allow(dead_code)]
    pub(super) fn shortcut(&self, keybinds: &Keybinds) -> Option<String> {
        let binding = self.binding?;
        (binding.keys)(keybinds).labels().into_iter().next()
    }
}

/// 某个动作此刻的状态。`visible` 为假 = 对这个对象 / 这个布局根本不适用（不列
/// 出）；`enabled` 为假 = 暂时不可用（菜单里置灰、搜索里不列出）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ActionState {
    pub(super) visible: bool,
    pub(super) enabled: bool,
    pub(super) checked: Option<bool>,
    pub(super) badge: bool,
    /// 换用 `alt_title` / `alt_label`。
    pub(super) alternate: bool,
}

impl ActionState {
    pub(super) const ENABLED: Self = Self {
        visible: true,
        enabled: true,
        checked: None,
        badge: false,
        alternate: false,
    };
    pub(super) const HIDDEN: Self = Self {
        visible: false,
        ..Self::ENABLED
    };

    pub(super) const fn enabled_if(enabled: bool) -> Self {
        Self {
            enabled,
            ..Self::ENABLED
        }
    }

    pub(super) const fn checked(checked: bool) -> Self {
        Self {
            checked: Some(checked),
            ..Self::ENABLED
        }
    }
}

/// 动作作用的对象。
#[derive(Debug, Clone)]
pub(super) enum ActionTarget {
    /// 当前聚焦的对象（与按快捷键同义）。
    Focused,
    Workspace {
        workspace_id: String,
    },
    Tab {
        tab_id: String,
        workspace_id: String,
    },
    Pane {
        pane_id: String,
        workspace_id: String,
        source_pane_id: Option<String>,
        right_click_passthrough: bool,
    },
    Machine(ClientEndpointId),
    Agent {
        endpoint_id: ClientEndpointId,
        owner: super::agent_activity_overlay::AgentActivityOwner,
    },
    Command(CustomCommandKeybind),
}

macro_rules! bind {
    ($action:ident, $field:ident) => {
        Some(ActionBinding {
            action: KeybindAction::$action,
            keys: |keys| &keys.$field,
        })
    };
}

const fn spec(
    id: ActionId,
    key: &'static str,
    title: TextKey,
    category: ActionCategory,
    group: u8,
) -> ActionSpec {
    ActionSpec {
        id,
        key,
        title,
        alt_title: None,
        label: None,
        alt_label: None,
        category,
        group,
        binding: None,
        palette: PaletteMode::Focused,
        mobile: false,
        context: None,
        danger: false,
    }
}

use ActionCategory as Cat;
use ActionId as Id;
use ClientContextMenuAction as Ctx;
use ContextKind as Kind;

/// 动作表。声明顺序即命令面板的列出顺序与模糊搜索同分时的先后，也是主菜单
/// 各分类子菜单里的顺序（同一分类的同组条目必须相邻）。
pub(super) static ACTIONS: &[ActionSpec] = &[
    // 需要注意的条目领头，不滚动也能看到。
    ActionSpec {
        alt_title: Some(|t| t.global_menu.update_ready),
        mobile: true,
        ..spec(
            Id::WhatsNew,
            "whats_new",
            |t| t.global_menu.whats_new,
            Cat::Help,
            0,
        )
    },
    ActionSpec {
        binding: bind!(Settings, settings),
        mobile: true,
        ..spec(
            Id::Settings,
            "binding:Settings",
            |t| t.global_menu.settings,
            Cat::Settings,
            0,
        )
    },
    ActionSpec {
        binding: bind!(ManageMachines, manage_machines),
        label: Some(|t| t.context_menu.manage_machines),
        mobile: true,
        context: Some((Kind::Machine, Ctx::ManageMachines)),
        ..spec(
            Id::ManageMachines,
            "binding:ManageMachines",
            |t| t.global_menu.machines,
            Cat::Machines,
            0,
        )
    },
    ActionSpec {
        mobile: true,
        ..spec(
            Id::Notifications,
            "notifications",
            |t| t.global_menu.notifications,
            Cat::Help,
            0,
        )
    },
    ActionSpec {
        binding: bind!(Help, help),
        mobile: true,
        ..spec(
            Id::Help,
            "binding:Help",
            |t| t.global_menu.keybinds,
            Cat::Help,
            0,
        )
    },
    ActionSpec {
        binding: bind!(ReloadConfig, reload_config),
        mobile: true,
        ..spec(
            Id::ReloadConfig,
            "binding:ReloadConfig",
            |t| t.global_menu.reload_config,
            Cat::Settings,
            0,
        )
    },
    ActionSpec {
        binding: bind!(WorkspacePicker, workspace_picker),
        ..spec(
            Id::WorkspacePicker,
            "binding:WorkspacePicker",
            |t| t.keybinds.workspace_navigation,
            Cat::Workspaces,
            0,
        )
    },
    ActionSpec {
        binding: bind!(OpenNavigator, goto),
        ..spec(
            Id::OpenNavigator,
            "binding:OpenNavigator",
            |t| t.keybinds.session_navigator,
            Cat::Workspaces,
            0,
        )
    },
    ActionSpec {
        binding: bind!(NewWorkspace, new_workspace),
        ..spec(
            Id::NewWorkspace,
            "binding:NewWorkspace",
            |t| t.keybinds.new_workspace,
            Cat::Workspaces,
            1,
        )
    },
    ActionSpec {
        binding: bind!(NewWorktree, new_worktree),
        label: Some(|t| t.context_menu.new_worktree),
        context: Some((Kind::Workspace, Ctx::NewWorktree)),
        ..spec(
            Id::NewWorktree,
            "binding:NewWorktree",
            |t| t.keybinds.new_worktree,
            Cat::Workspaces,
            1,
        )
    },
    ActionSpec {
        binding: bind!(OpenWorktree, open_worktree),
        label: Some(|t| t.context_menu.open_worktree),
        context: Some((Kind::Workspace, Ctx::OpenWorktree)),
        ..spec(
            Id::OpenWorktree,
            "binding:OpenWorktree",
            |t| t.keybinds.open_worktree,
            Cat::Workspaces,
            1,
        )
    },
    ActionSpec {
        binding: bind!(RemoveWorktree, remove_worktree),
        label: Some(|t| t.context_menu.delete_worktree),
        context: Some((Kind::Workspace, Ctx::RemoveWorktree)),
        danger: true,
        ..spec(
            Id::RemoveWorktree,
            "binding:RemoveWorktree",
            |t| t.keybinds.delete_worktree_checkout,
            Cat::Workspaces,
            1,
        )
    },
    ActionSpec {
        binding: bind!(RenameWorkspace, rename_workspace),
        label: Some(|t| t.context_menu.rename),
        context: Some((Kind::Workspace, Ctx::Rename)),
        ..spec(
            Id::RenameWorkspace,
            "binding:RenameWorkspace",
            |t| t.keybinds.rename_workspace,
            Cat::Workspaces,
            1,
        )
    },
    ActionSpec {
        binding: bind!(CloseWorkspace, close_workspace),
        label: Some(|t| t.context_menu.close),
        alt_label: Some(|t| t.context_menu.close_group),
        context: Some((Kind::Workspace, Ctx::Close)),
        danger: true,
        ..spec(
            Id::CloseWorkspace,
            "binding:CloseWorkspace",
            |t| t.keybinds.close_workspace,
            Cat::Workspaces,
            1,
        )
    },
    ActionSpec {
        palette: PaletteMode::Hidden,
        context: Some((Kind::Workspace, Ctx::ToggleGroup)),
        ..spec(
            Id::ToggleWorkspaceGroup,
            "workspace:toggle-group",
            |t| t.context_menu.collapse_group,
            Cat::Workspaces,
            1,
        )
    },
    ActionSpec {
        binding: bind!(PreviousWorkspace, previous_workspace),
        ..spec(
            Id::PreviousWorkspace,
            "binding:PreviousWorkspace",
            |t| t.keybinds.previous_workspace,
            Cat::Workspaces,
            2,
        )
    },
    ActionSpec {
        binding: bind!(NextWorkspace, next_workspace),
        ..spec(
            Id::NextWorkspace,
            "binding:NextWorkspace",
            |t| t.keybinds.next_workspace,
            Cat::Workspaces,
            2,
        )
    },
    ActionSpec {
        binding: bind!(PreviousAgent, previous_agent),
        ..spec(
            Id::PreviousAgent,
            "binding:PreviousAgent",
            |t| t.keybinds.previous_agent,
            Cat::Workspaces,
            2,
        )
    },
    ActionSpec {
        binding: bind!(NextAgent, next_agent),
        ..spec(
            Id::NextAgent,
            "binding:NextAgent",
            |t| t.keybinds.next_agent,
            Cat::Workspaces,
            2,
        )
    },
    ActionSpec {
        binding: bind!(NewTab, new_tab),
        label: Some(|t| t.context_menu.new_tab),
        context: Some((Kind::Tab, Ctx::NewTab)),
        ..spec(
            Id::NewTab,
            "binding:NewTab",
            |t| t.keybinds.new_tab,
            Cat::TabsPanes,
            0,
        )
    },
    ActionSpec {
        binding: bind!(RenameTab, rename_tab),
        label: Some(|t| t.context_menu.rename),
        context: Some((Kind::Tab, Ctx::Rename)),
        ..spec(
            Id::RenameTab,
            "binding:RenameTab",
            |t| t.keybinds.rename_tab,
            Cat::TabsPanes,
            0,
        )
    },
    ActionSpec {
        binding: bind!(PreviousTab, previous_tab),
        ..spec(
            Id::PreviousTab,
            "binding:PreviousTab",
            |t| t.keybinds.previous_tab,
            Cat::TabsPanes,
            0,
        )
    },
    ActionSpec {
        binding: bind!(NextTab, next_tab),
        ..spec(
            Id::NextTab,
            "binding:NextTab",
            |t| t.keybinds.next_tab,
            Cat::TabsPanes,
            0,
        )
    },
    ActionSpec {
        binding: bind!(MoveTabPrevious, move_tab_previous),
        ..spec(
            Id::MoveTabPrevious,
            "binding:MoveTabPrevious",
            |t| t.keybinds.move_tab_left,
            Cat::TabsPanes,
            0,
        )
    },
    ActionSpec {
        binding: bind!(MoveTabNext, move_tab_next),
        ..spec(
            Id::MoveTabNext,
            "binding:MoveTabNext",
            |t| t.keybinds.move_tab_right,
            Cat::TabsPanes,
            0,
        )
    },
    ActionSpec {
        binding: bind!(CloseTab, close_tab),
        label: Some(|t| t.context_menu.close),
        context: Some((Kind::Tab, Ctx::Close)),
        danger: true,
        ..spec(
            Id::CloseTab,
            "binding:CloseTab",
            |t| t.keybinds.close_tab,
            Cat::TabsPanes,
            0,
        )
    },
    ActionSpec {
        binding: bind!(RenamePane, rename_pane),
        label: Some(|t| t.context_menu.rename_pane),
        context: Some((Kind::Pane, Ctx::RenamePane)),
        ..spec(
            Id::RenamePane,
            "binding:RenamePane",
            |t| t.keybinds.rename_pane,
            Cat::TabsPanes,
            1,
        )
    },
    ActionSpec {
        palette: PaletteMode::Hidden,
        context: Some((Kind::Pane, Ctx::ClearPaneName)),
        ..spec(
            Id::ClearPaneName,
            "pane:clear-name",
            |t| t.context_menu.clear_pane_name,
            Cat::TabsPanes,
            1,
        )
    },
    ActionSpec {
        palette: PaletteMode::Hidden,
        context: Some((Kind::Pane, Ctx::SwapWithFocusedPane)),
        ..spec(
            Id::SwapWithFocusedPane,
            "pane:swap-with-focused",
            |t| t.context_menu.swap_with_focused,
            Cat::TabsPanes,
            1,
        )
    },
    ActionSpec {
        binding: bind!(EditScrollback, edit_scrollback),
        ..spec(
            Id::EditScrollback,
            "binding:EditScrollback",
            |t| t.keybinds.edit_scrollback,
            Cat::TabsPanes,
            1,
        )
    },
    ActionSpec {
        binding: bind!(CopyMode, copy_mode),
        ..spec(
            Id::CopyMode,
            "binding:CopyMode",
            |t| t.keybinds.copy_mode,
            Cat::TabsPanes,
            1,
        )
    },
    ActionSpec {
        binding: bind!(SplitVertical, split_vertical),
        label: Some(|t| t.context_menu.split_right),
        context: Some((Kind::Pane, Ctx::SplitRight)),
        ..spec(
            Id::SplitRight,
            "binding:SplitVertical",
            |t| t.keybinds.split_vertical,
            Cat::TabsPanes,
            1,
        )
    },
    ActionSpec {
        binding: bind!(SplitHorizontal, split_horizontal),
        label: Some(|t| t.context_menu.split_down),
        context: Some((Kind::Pane, Ctx::SplitDown)),
        ..spec(
            Id::SplitDown,
            "binding:SplitHorizontal",
            |t| t.keybinds.split_horizontal,
            Cat::TabsPanes,
            1,
        )
    },
    ActionSpec {
        binding: bind!(ClosePane, close_pane),
        label: Some(|t| t.context_menu.close_pane),
        context: Some((Kind::Pane, Ctx::ClosePane)),
        danger: true,
        ..spec(
            Id::ClosePane,
            "binding:ClosePane",
            |t| t.keybinds.close_pane,
            Cat::TabsPanes,
            1,
        )
    },
    ActionSpec {
        binding: bind!(Zoom, zoom),
        label: Some(|t| t.context_menu.zoom),
        context: Some((Kind::Pane, Ctx::Zoom)),
        ..spec(
            Id::Zoom,
            "binding:Zoom",
            |t| t.keybinds.zoom_pane,
            Cat::TabsPanes,
            1,
        )
    },
    ActionSpec {
        palette: PaletteMode::Hidden,
        context: Some((Kind::Pane, Ctx::ToggleRightClickPassthrough)),
        ..spec(
            Id::RightClickPassthrough,
            "pane:right-click-passthrough",
            |t| t.context_menu.send_right_clicks,
            Cat::TabsPanes,
            1,
        )
    },
    ActionSpec {
        binding: bind!(EnterResizeMode, resize_mode),
        ..spec(
            Id::EnterResizeMode,
            "binding:EnterResizeMode",
            |t| t.keybinds.resize_mode,
            Cat::TabsPanes,
            1,
        )
    },
    ActionSpec {
        binding: bind!(FocusPaneLeft, focus_pane_left),
        ..spec(
            Id::FocusPaneLeft,
            "binding:FocusPaneLeft",
            |t| t.keybinds.focus_pane_left,
            Cat::TabsPanes,
            2,
        )
    },
    ActionSpec {
        binding: bind!(FocusPaneDown, focus_pane_down),
        ..spec(
            Id::FocusPaneDown,
            "binding:FocusPaneDown",
            |t| t.keybinds.focus_pane_down,
            Cat::TabsPanes,
            2,
        )
    },
    ActionSpec {
        binding: bind!(FocusPaneUp, focus_pane_up),
        ..spec(
            Id::FocusPaneUp,
            "binding:FocusPaneUp",
            |t| t.keybinds.focus_pane_up,
            Cat::TabsPanes,
            2,
        )
    },
    ActionSpec {
        binding: bind!(FocusPaneRight, focus_pane_right),
        ..spec(
            Id::FocusPaneRight,
            "binding:FocusPaneRight",
            |t| t.keybinds.focus_pane_right,
            Cat::TabsPanes,
            2,
        )
    },
    ActionSpec {
        binding: bind!(CyclePaneNext, cycle_pane_next),
        ..spec(
            Id::CyclePaneNext,
            "binding:CyclePaneNext",
            |t| t.keybinds.cycle_pane_next,
            Cat::TabsPanes,
            2,
        )
    },
    ActionSpec {
        binding: bind!(CyclePanePrevious, cycle_pane_previous),
        ..spec(
            Id::CyclePanePrevious,
            "binding:CyclePanePrevious",
            |t| t.keybinds.cycle_pane_previous,
            Cat::TabsPanes,
            2,
        )
    },
    ActionSpec {
        binding: bind!(LastPane, last_pane),
        ..spec(
            Id::LastPane,
            "binding:LastPane",
            |t| t.keybinds.last_pane,
            Cat::TabsPanes,
            2,
        )
    },
    ActionSpec {
        binding: bind!(LinkHints, link_hints),
        ..spec(
            Id::LinkHints,
            "binding:LinkHints",
            |t| t.keybinds.link_hints,
            Cat::Help,
            1,
        )
    },
    ActionSpec {
        binding: bind!(ToggleSidebar, toggle_sidebar),
        ..spec(
            Id::ToggleSidebar,
            "binding:ToggleSidebar",
            |t| t.keybinds.toggle_sidebar,
            Cat::Settings,
            0,
        )
    },
    ActionSpec {
        binding: bind!(Detach, detach),
        mobile: true,
        ..spec(
            Id::Detach,
            "binding:Detach",
            |t| t.global_menu.detach,
            Cat::Help,
            2,
        )
    },
    // 标题由配置给出（描述或命令本身），这里的文案不会被读到。
    ActionSpec {
        palette: PaletteMode::PerCommand,
        ..spec(
            Id::CustomCommand,
            "command",
            |t| t.global_menu.command_search,
            Cat::Tools,
            0,
        )
    },
    ActionSpec {
        ..spec(
            Id::MachineImport,
            "machine:import",
            |t| t.global_menu.machine_import,
            Cat::Machines,
            0,
        )
    },
    ActionSpec {
        ..spec(
            Id::Snippets,
            "snippets:list",
            |t| t.global_menu.snippets,
            Cat::Tools,
            1,
        )
    },
    ActionSpec {
        ..spec(
            Id::SnippetRun,
            "snippets:run",
            |t| t.global_menu.snippet_run,
            Cat::Tools,
            1,
        )
    },
    ActionSpec {
        ..spec(
            Id::SceneSave,
            "scene:save",
            |t| t.global_menu.scene_save,
            Cat::Tools,
            2,
        )
    },
    ActionSpec {
        ..spec(
            Id::SceneRestore,
            "scene:list",
            |t| t.global_menu.scene_restore,
            Cat::Tools,
            2,
        )
    },
    ActionSpec {
        ..spec(
            Id::Broadcast,
            "broadcast",
            |t| t.global_menu.broadcast,
            Cat::Tools,
            3,
        )
    },
    // 机器动作组：命令面板按机器铺开（机器之间按机器分组），机器行右键按对象取。
    ActionSpec {
        palette: PaletteMode::PerMachine,
        label: Some(|t| t.context_menu.reconnect_machine),
        context: Some((Kind::Machine, Ctx::ReconnectMachine)),
        ..spec(
            Id::MachineConnect,
            "machine:connect",
            |t| t.global_menu.machine_connect_fmt,
            Cat::Machines,
            1,
        )
    },
    ActionSpec {
        palette: PaletteMode::PerMachine,
        label: Some(|t| t.context_menu.switch_machine),
        ..spec(
            Id::MachineSwitch,
            "machine:switch",
            |t| t.global_menu.machine_switch_fmt,
            Cat::Machines,
            1,
        )
    },
    ActionSpec {
        palette: PaletteMode::PerMachine,
        label: Some(|t| t.context_menu.rename),
        context: Some((Kind::Machine, Ctx::RenameMachine)),
        ..spec(
            Id::MachineRename,
            "machine:rename",
            |t| t.global_menu.machine_rename_fmt,
            Cat::Machines,
            1,
        )
    },
    ActionSpec {
        palette: PaletteMode::PerMachine,
        label: Some(|t| t.context_menu.edit_machine),
        context: Some((Kind::Machine, Ctx::EditMachine)),
        ..spec(
            Id::MachineEdit,
            "machine:edit",
            |t| t.global_menu.machine_edit_fmt,
            Cat::Machines,
            1,
        )
    },
    ActionSpec {
        palette: PaletteMode::PerMachine,
        label: Some(|t| t.context_menu.enable_machine),
        context: Some((Kind::Machine, Ctx::ToggleMachineEnabled)),
        ..spec(
            Id::MachineEnabled,
            "machine:toggle",
            |t| t.global_menu.machine_enable_fmt,
            Cat::Machines,
            1,
        )
    },
    ActionSpec {
        palette: PaletteMode::PerMachine,
        label: Some(|t| t.context_menu.copy_machine_fix_command),
        context: Some((Kind::Machine, Ctx::CopyMachineFixCommand)),
        ..spec(
            Id::MachineCopyFixCommand,
            "machine:copy-fix",
            |t| t.global_menu.machine_copy_fix_fmt,
            Cat::Machines,
            1,
        )
    },
    ActionSpec {
        palette: PaletteMode::PerMachine,
        label: Some(|t| t.context_menu.remove_machine),
        context: Some((Kind::Machine, Ctx::RemoveMachine)),
        danger: true,
        ..spec(
            Id::MachineRemove,
            "machine:remove",
            |t| t.global_menu.machine_remove_fmt,
            Cat::Machines,
            1,
        )
    },
    ActionSpec {
        label: Some(|t| t.menu.monitor_short),
        mobile: true,
        ..spec(
            Id::Monitor,
            "observation:monitor",
            |t| t.menu.monitor,
            Cat::Monitor,
            0,
        )
    },
    ActionSpec {
        ..spec(
            Id::CloseMonitor,
            "observation:close-monitor",
            |t| t.menu.close_monitor,
            Cat::Monitor,
            0,
        )
    },
    ActionSpec {
        ..spec(
            Id::ArrangeLayout,
            "layout",
            |t| t.menu.arrange_layout,
            Cat::TabsPanes,
            3,
        )
    },
    ActionSpec {
        ..spec(
            Id::LockLayout,
            "layout:lock",
            |t| t.menu.lock_layout,
            Cat::TabsPanes,
            3,
        )
    },
    // Agent 行右键菜单（条目由 `context_menu.rs::items` 的 Agent 臂给出，执行由
    // 面板车道的 `activate_agent_context_action` 承接）。
    ActionSpec {
        palette: PaletteMode::Hidden,
        context: Some((Kind::Agent, Ctx::FocusAgent)),
        ..spec(
            Id::AgentFocus,
            "agent:focus",
            |t| t.agent_panel.menu_focus,
            Cat::Workspaces,
            3,
        )
    },
    ActionSpec {
        palette: PaletteMode::Hidden,
        context: Some((Kind::Agent, Ctx::ViewAgentActivity)),
        ..spec(
            Id::AgentViewActivity,
            "agent:view-activity",
            |t| t.agent_panel.menu_view_activity,
            Cat::Workspaces,
            3,
        )
    },
    ActionSpec {
        palette: PaletteMode::Hidden,
        context: Some((Kind::Agent, Ctx::RenameAgent)),
        ..spec(
            Id::AgentRename,
            "agent:rename",
            |t| t.agent_panel.menu_rename,
            Cat::Workspaces,
            3,
        )
    },
    ActionSpec {
        palette: PaletteMode::Hidden,
        context: Some((Kind::Agent, Ctx::ShowAgentUsage)),
        ..spec(
            Id::AgentUsage,
            "agent:usage",
            |t| t.agent_panel.menu_usage,
            Cat::Workspaces,
            3,
        )
    },
    ActionSpec {
        palette: PaletteMode::Hidden,
        context: Some((Kind::Agent, Ctx::BindAgentAccount)),
        ..spec(
            Id::AgentBindAccount,
            "agent:bind-account",
            |t| t.agent_panel.menu_bind_account,
            Cat::Workspaces,
            3,
        )
    },
    ActionSpec {
        palette: PaletteMode::Hidden,
        context: Some((Kind::Agent, Ctx::CloseAgentPane)),
        danger: true,
        ..spec(
            Id::AgentClose,
            "agent:close",
            |t| t.agent_panel.menu_close,
            Cat::Workspaces,
            3,
        )
    },
];

/// 按 id 取表项。表由单测守门（每个 id 恰好一条），查不到只可能是新增变体
/// 忘了登记——落回第一条，保证渲染不 panic。
pub(super) fn action_spec(id: ActionId) -> &'static ActionSpec {
    ACTIONS
        .iter()
        .find(|spec| spec.id == id)
        .unwrap_or(&ACTIONS[0])
}

/// 右键菜单里 (对象种类, 菜单动作) 对应的动作表条目。
pub(super) fn context_action(
    kind: ContextKind,
    action: ClientContextMenuAction,
) -> Option<ActionId> {
    ACTIONS
        .iter()
        .find(|spec| spec.context == Some((kind, action)))
        .map(|spec| spec.id)
}

/// 全局动作判定所需的上下文。mobile 渲染层只有快照，布局相关字段取假。
#[derive(Clone, Copy, Default)]
pub(super) struct GlobalActionContext<'a> {
    pub(super) snapshot: Option<&'a ClientShellSnapshot>,
    pub(super) workbench: bool,
    pub(super) monitor_docked: bool,
    pub(super) layout_locked: bool,
}

impl<'a> GlobalActionContext<'a> {
    pub(super) fn from_snapshot(snapshot: &'a ClientShellSnapshot) -> Self {
        Self {
            snapshot: Some(snapshot),
            ..Self::default()
        }
    }
}

/// 作用在「当前聚焦对象」上的动作此刻的状态（命令面板、主菜单、mobile 菜单）。
pub(super) fn global_action_state(id: ActionId, cx: &GlobalActionContext<'_>) -> ActionState {
    match id {
        ActionId::WhatsNew => {
            let update = cx
                .snapshot
                .is_some_and(|snapshot| snapshot.update_available.is_some());
            let notes = cx
                .snapshot
                .is_some_and(|snapshot| snapshot.latest_release_notes_available);
            if !update && !notes {
                return ActionState::HIDDEN;
            }
            ActionState {
                badge: update,
                alternate: update,
                // 发行说明正文还没随快照到达时点了也打不开。
                enabled: cx
                    .snapshot
                    .is_some_and(|snapshot| snapshot.release_notes.is_some()),
                ..ActionState::ENABLED
            }
        }
        ActionId::Settings => ActionState {
            badge: cx
                .snapshot
                .is_some_and(|snapshot| snapshot.integration_updates_available),
            ..ActionState::ENABLED
        },
        ActionId::CloseMonitor => {
            if !cx.workbench {
                return ActionState::HIDDEN;
            }
            ActionState::enabled_if(cx.monitor_docked)
        }
        ActionId::ArrangeLayout if !cx.workbench => ActionState::HIDDEN,
        ActionId::LockLayout => {
            if !cx.workbench {
                return ActionState::HIDDEN;
            }
            ActionState::checked(cx.layout_locked)
        }
        _ => ActionState::ENABLED,
    }
}

/// 机器动作组对某台已保存机器的状态（命令面板按机器铺开、机器行右键共用）。
pub(super) fn machine_action_state(
    id: ActionId,
    profile_enabled: bool,
    online: bool,
    active: bool,
) -> ActionState {
    match id {
        ActionId::MachineConnect => ActionState::enabled_if(profile_enabled && !online),
        ActionId::MachineSwitch => ActionState::enabled_if(profile_enabled && online && !active),
        ActionId::MachineEnabled => ActionState::checked(profile_enabled),
        _ => ActionState::ENABLED,
    }
}

impl ClientShellState {
    pub(super) fn global_action_context(&self) -> GlobalActionContext<'_> {
        GlobalActionContext {
            snapshot: self.snapshot.as_deref(),
            workbench: self.workbench.enabled,
            monitor_docked: self.workbench.enabled
                && self
                    .workbench
                    .dock
                    .root
                    .contains(&super::dock::PanelId::Monitor),
            layout_locked: self.workbench.dock.locked,
        }
    }

    /// 执行一个动作。穷尽 match：每个 [`ActionId`] 都在这里有且只有一个分支。
    /// 对象不匹配的组合（如对工作区目标执行分屏）静默忽略。
    pub(super) fn run_action(
        &mut self,
        id: ActionId,
        target: ActionTarget,
        outcome: &mut ClientShellInput,
    ) {
        use crate::api::schema::{
            Method, PaneInputSetParams, PaneRenameParams, PaneRightClickTarget, PaneSplitParams,
            PaneSwapParams, PaneTarget, PaneZoomMode, PaneZoomParams, SplitDirection, TabTarget,
        };

        // 快捷键语义：作用在聚焦对象上的动作与按键走同一条路径。
        if let ActionTarget::Focused = target {
            if let Some(binding) = action_spec(id).binding {
                self.record_binding(crate::input::KeybindMatch::Action(binding.action), outcome);
                return;
            }
        }
        match id {
            // 以下动作只有「聚焦对象」语义，已由上面的快捷键路径执行。
            ActionId::Settings
            | ActionId::Help
            | ActionId::ReloadConfig
            | ActionId::WorkspacePicker
            | ActionId::OpenNavigator
            | ActionId::NewWorkspace
            | ActionId::PreviousWorkspace
            | ActionId::NextWorkspace
            | ActionId::PreviousAgent
            | ActionId::NextAgent
            | ActionId::PreviousTab
            | ActionId::NextTab
            | ActionId::MoveTabPrevious
            | ActionId::MoveTabNext
            | ActionId::EditScrollback
            | ActionId::CopyMode
            | ActionId::EnterResizeMode
            | ActionId::FocusPaneLeft
            | ActionId::FocusPaneDown
            | ActionId::FocusPaneUp
            | ActionId::FocusPaneRight
            | ActionId::CyclePaneNext
            | ActionId::CyclePanePrevious
            | ActionId::LastPane
            | ActionId::LinkHints
            | ActionId::ToggleSidebar
            | ActionId::Detach => {}
            ActionId::WhatsNew => self.open_release_notes(),
            ActionId::Notifications => self.open_notification_history(),
            ActionId::ManageMachines => self.open_machines_overlay(),
            ActionId::MachineImport => self.open_machine_import_wizard(),
            ActionId::Snippets => self.open_snippets_overlay(false),
            ActionId::SnippetRun => self.open_snippets_overlay(true),
            ActionId::SceneSave => self.open_scenes_overlay_saving(),
            ActionId::SceneRestore => self.open_scenes_overlay(),
            ActionId::Broadcast => self.open_broadcast_overlay(),
            ActionId::Monitor => {
                self.open_observation_page(super::observability::Page::Monitor, outcome)
            }
            ActionId::CloseMonitor => {
                self.close_workbench_panel(super::dock::PanelId::Monitor, outcome);
            }
            ActionId::ArrangeLayout => self.workbench.arranging = true,
            ActionId::LockLayout => {
                self.workbench.dock.locked = !self.workbench.dock.locked;
                self.schedule_chrome_preferences(std::time::Instant::now());
            }
            ActionId::CustomCommand => {
                if let ActionTarget::Command(command) = target {
                    self.record_binding(crate::input::KeybindMatch::Command(command), outcome);
                }
            }
            ActionId::RenameWorkspace => {
                let ActionTarget::Workspace { workspace_id } = target else {
                    return;
                };
                let label = self
                    .snapshot
                    .as_deref()
                    .and_then(|snapshot| {
                        snapshot
                            .workspaces
                            .iter()
                            .find(|workspace| workspace.workspace_id == workspace_id)
                    })
                    .map(|workspace| workspace.label.clone());
                if let Some(label) = label {
                    self.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
                        title: "rename workspace",
                        input: TextEditor::new(&label, false),
                        target: ClientRenameTarget::Workspace { workspace_id },
                    }));
                }
            }
            ActionId::CloseWorkspace => {
                let ActionTarget::Workspace { workspace_id } = target else {
                    return;
                };
                if self.config.confirm_close {
                    self.open_confirm_close_overlay(workspace_id);
                } else {
                    self.push_endpoint_method(
                        Method::WorkspaceClose(crate::api::schema::WorkspaceCloseParams {
                            workspace_id,
                            close_group: true,
                        }),
                        outcome,
                    );
                }
            }
            ActionId::NewWorktree | ActionId::OpenWorktree | ActionId::RemoveWorktree => {
                let ActionTarget::Workspace { workspace_id } = target else {
                    return;
                };
                let binding = match id {
                    ActionId::NewWorktree => KeybindAction::NewWorktree,
                    ActionId::OpenWorktree => KeybindAction::OpenWorktree,
                    _ => KeybindAction::RemoveWorktree,
                };
                self.begin_worktree_action_for(binding, workspace_id, outcome);
            }
            ActionId::ToggleWorkspaceGroup => {
                let ActionTarget::Workspace { workspace_id } = target else {
                    return;
                };
                let key = self.snapshot.as_deref().and_then(|snapshot| {
                    snapshot
                        .workspaces
                        .iter()
                        .find(|workspace| workspace.workspace_id == workspace_id)
                        .and_then(|workspace| workspace.worktree.as_ref())
                        .map(|worktree| worktree.key.clone())
                });
                if let Some(key) = key {
                    let endpoint_id = self.active_endpoint_id.clone();
                    self.toggle_collapsed_group(&endpoint_id, key);
                    self.persist_chrome_preferences(outcome);
                }
            }
            ActionId::NewTab | ActionId::RenameTab | ActionId::CloseTab => {
                let ActionTarget::Tab {
                    tab_id,
                    workspace_id,
                } = target
                else {
                    return;
                };
                self.push_endpoint_method(
                    Method::TabFocus(TabTarget {
                        tab_id: tab_id.clone(),
                    }),
                    outcome,
                );
                match id {
                    ActionId::NewTab => self.new_tab_in(workspace_id, outcome),
                    ActionId::RenameTab => self.open_tab_rename(tab_id),
                    _ => self.push_endpoint_method(Method::TabClose(TabTarget { tab_id }), outcome),
                }
            }
            ActionId::RenamePane => {
                let ActionTarget::Pane { pane_id, .. } = target else {
                    return;
                };
                let label = self.snapshot.as_deref().and_then(|snapshot| {
                    snapshot
                        .panes
                        .iter()
                        .find(|pane| pane.pane_id == pane_id)
                        .and_then(|pane| pane.label.clone())
                });
                self.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
                    title: "rename pane",
                    input: TextEditor::new(label.as_deref().unwrap_or_default(), label.is_none()),
                    target: ClientRenameTarget::Pane { pane_id },
                }));
            }
            ActionId::ClearPaneName => {
                let ActionTarget::Pane { pane_id, .. } = target else {
                    return;
                };
                self.push_endpoint_method(
                    Method::PaneRename(PaneRenameParams {
                        pane_id,
                        label: None,
                    }),
                    outcome,
                );
            }
            ActionId::SwapWithFocusedPane => {
                let ActionTarget::Pane {
                    pane_id,
                    source_pane_id: Some(source_pane_id),
                    ..
                } = target
                else {
                    return;
                };
                self.push_endpoint_method(
                    Method::PaneSwap(PaneSwapParams {
                        pane_id: None,
                        direction: None,
                        source_pane_id: Some(source_pane_id.clone()),
                        target_pane_id: Some(pane_id),
                    }),
                    outcome,
                );
                self.push_endpoint_method(
                    Method::PaneFocus(PaneTarget {
                        pane_id: source_pane_id,
                    }),
                    outcome,
                );
            }
            ActionId::SplitRight | ActionId::SplitDown => {
                let ActionTarget::Pane {
                    pane_id,
                    workspace_id,
                    ..
                } = target
                else {
                    return;
                };
                self.push_endpoint_method(
                    Method::PaneSplit(PaneSplitParams {
                        workspace_id: Some(workspace_id),
                        target_pane_id: Some(pane_id),
                        direction: if id == ActionId::SplitRight {
                            SplitDirection::Right
                        } else {
                            SplitDirection::Down
                        },
                        ratio: None,
                        cwd: None,
                        focus: true,
                        right_click: Default::default(),
                        env: Default::default(),
                    }),
                    outcome,
                );
            }
            ActionId::Zoom => {
                let ActionTarget::Pane { pane_id, .. } = target else {
                    return;
                };
                self.push_endpoint_method(
                    Method::PaneZoom(PaneZoomParams {
                        pane_id: Some(pane_id),
                        mode: PaneZoomMode::Toggle,
                    }),
                    outcome,
                );
            }
            ActionId::RightClickPassthrough => {
                let ActionTarget::Pane {
                    pane_id,
                    right_click_passthrough,
                    ..
                } = target
                else {
                    return;
                };
                self.push_endpoint_method(
                    Method::PaneInputSet(PaneInputSetParams {
                        pane_id,
                        right_click: if right_click_passthrough {
                            PaneRightClickTarget::Herdr
                        } else {
                            PaneRightClickTarget::Pane
                        },
                    }),
                    outcome,
                );
            }
            ActionId::ClosePane => {
                let ActionTarget::Pane { pane_id, .. } = target else {
                    return;
                };
                self.push_endpoint_method(Method::PaneClose(PaneTarget { pane_id }), outcome);
            }
            ActionId::MachineSwitch => {
                if let ActionTarget::Machine(endpoint_id) = target {
                    self.activate_endpoint(endpoint_id, outcome);
                }
            }
            ActionId::MachineConnect
            | ActionId::MachineRename
            | ActionId::MachineEdit
            | ActionId::MachineEnabled
            | ActionId::MachineCopyFixCommand
            | ActionId::MachineRemove => {
                let ActionTarget::Machine(ClientEndpointId::Ssh(profile_id)) = target else {
                    return;
                };
                self.run_machine_action(id, profile_id, outcome);
            }
            ActionId::AgentFocus
            | ActionId::AgentViewActivity
            | ActionId::AgentRename
            | ActionId::AgentUsage
            | ActionId::AgentBindAccount
            | ActionId::AgentClose => {
                let ActionTarget::Agent { endpoint_id, owner } = target else {
                    return;
                };
                let Some((_, action)) = action_spec(id).context else {
                    return;
                };
                self.activate_agent_context_action(endpoint_id, owner, action, outcome);
            }
        }
    }

    fn run_machine_action(
        &mut self,
        id: ActionId,
        profile_id: crate::client::endpoint::ProfileId,
        outcome: &mut ClientShellInput,
    ) {
        match id {
            ActionId::MachineConnect => self.machine_reconnect(&profile_id, outcome),
            ActionId::MachineRename => {
                let label = self
                    .saved_profiles
                    .iter()
                    .find(|profile| profile.id == profile_id)
                    .map(|profile| profile.label.clone());
                if let Some(label) = label {
                    self.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
                        title: crate::i18n::texts().machines.edit_title,
                        input: TextEditor::new(&label, false),
                        target: ClientRenameTarget::Machine { profile_id },
                    }));
                }
            }
            ActionId::MachineEdit => self.open_machine_edit_form(&profile_id),
            ActionId::MachineEnabled => {
                let enabled = self
                    .saved_profiles
                    .iter()
                    .any(|profile| profile.id == profile_id && profile.enabled);
                self.machine_set_enabled(&profile_id, !enabled);
            }
            ActionId::MachineRemove => self.open_machine_remove_confirm(&profile_id),
            // 与机器面板的 `c` 同一条路径：自己拼命令会漏掉反馈（C-27）。
            ActionId::MachineCopyFixCommand => self.machine_copy_fix_command(&profile_id, outcome),
            _ => {}
        }
    }

    fn new_tab_in(&mut self, workspace_id: String, outcome: &mut ClientShellInput) {
        if self.config.prompt_new_tab_name {
            let default_name = (self
                .snapshot
                .as_deref()
                .map(|snapshot| {
                    snapshot
                        .tabs
                        .iter()
                        .filter(|tab| tab.workspace_id == workspace_id)
                        .count()
                })
                .unwrap_or(0)
                + 1)
            .to_string();
            self.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
                title: "new tab",
                input: TextEditor::new(&default_name, true),
                target: ClientRenameTarget::NewTab {
                    workspace_id,
                    default_name,
                },
            }));
        } else {
            self.push_endpoint_method(
                crate::api::schema::Method::TabCreate(crate::api::schema::TabCreateParams {
                    workspace_id: Some(workspace_id),
                    cwd: None,
                    focus: true,
                    label: None,
                    env: Default::default(),
                }),
                outcome,
            );
        }
    }

    fn open_tab_rename(&mut self, tab_id: String) {
        let tab = self
            .snapshot
            .as_deref()
            .and_then(|snapshot| snapshot.tabs.iter().find(|tab| tab.tab_id == tab_id));
        if let Some(tab) = tab {
            self.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
                title: "rename tab",
                input: TextEditor::new(&tab.label, false),
                target: ClientRenameTarget::Tab {
                    tab_id,
                    auto_name: !tab.custom_label,
                    original_name: tab.label.clone(),
                },
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_IDS: &[ActionId] = &[
        ActionId::WhatsNew,
        ActionId::Settings,
        ActionId::ManageMachines,
        ActionId::Notifications,
        ActionId::Help,
        ActionId::ReloadConfig,
        ActionId::WorkspacePicker,
        ActionId::OpenNavigator,
        ActionId::NewWorkspace,
        ActionId::NewWorktree,
        ActionId::OpenWorktree,
        ActionId::RemoveWorktree,
        ActionId::RenameWorkspace,
        ActionId::CloseWorkspace,
        ActionId::ToggleWorkspaceGroup,
        ActionId::PreviousWorkspace,
        ActionId::NextWorkspace,
        ActionId::PreviousAgent,
        ActionId::NextAgent,
        ActionId::NewTab,
        ActionId::RenameTab,
        ActionId::PreviousTab,
        ActionId::NextTab,
        ActionId::MoveTabPrevious,
        ActionId::MoveTabNext,
        ActionId::CloseTab,
        ActionId::RenamePane,
        ActionId::ClearPaneName,
        ActionId::SwapWithFocusedPane,
        ActionId::EditScrollback,
        ActionId::CopyMode,
        ActionId::SplitRight,
        ActionId::SplitDown,
        ActionId::ClosePane,
        ActionId::Zoom,
        ActionId::RightClickPassthrough,
        ActionId::EnterResizeMode,
        ActionId::FocusPaneLeft,
        ActionId::FocusPaneDown,
        ActionId::FocusPaneUp,
        ActionId::FocusPaneRight,
        ActionId::CyclePaneNext,
        ActionId::CyclePanePrevious,
        ActionId::LastPane,
        ActionId::LinkHints,
        ActionId::ToggleSidebar,
        ActionId::Detach,
        ActionId::CustomCommand,
        ActionId::MachineImport,
        ActionId::Snippets,
        ActionId::SnippetRun,
        ActionId::SceneSave,
        ActionId::SceneRestore,
        ActionId::Broadcast,
        ActionId::MachineConnect,
        ActionId::MachineSwitch,
        ActionId::MachineRename,
        ActionId::MachineEdit,
        ActionId::MachineEnabled,
        ActionId::MachineCopyFixCommand,
        ActionId::MachineRemove,
        ActionId::Monitor,
        ActionId::CloseMonitor,
        ActionId::ArrangeLayout,
        ActionId::LockLayout,
        ActionId::AgentFocus,
        ActionId::AgentViewActivity,
        ActionId::AgentRename,
        ActionId::AgentUsage,
        ActionId::AgentBindAccount,
        ActionId::AgentClose,
    ];

    /// 穷尽 match 让新增变体在这里编译不过，逼着同时补 `ALL_IDS`。
    fn listed(id: ActionId) -> bool {
        match id {
            ActionId::WhatsNew
            | ActionId::Settings
            | ActionId::ManageMachines
            | ActionId::Notifications
            | ActionId::Help
            | ActionId::ReloadConfig
            | ActionId::WorkspacePicker
            | ActionId::OpenNavigator
            | ActionId::NewWorkspace
            | ActionId::NewWorktree
            | ActionId::OpenWorktree
            | ActionId::RemoveWorktree
            | ActionId::RenameWorkspace
            | ActionId::CloseWorkspace
            | ActionId::ToggleWorkspaceGroup
            | ActionId::PreviousWorkspace
            | ActionId::NextWorkspace
            | ActionId::PreviousAgent
            | ActionId::NextAgent
            | ActionId::NewTab
            | ActionId::RenameTab
            | ActionId::PreviousTab
            | ActionId::NextTab
            | ActionId::MoveTabPrevious
            | ActionId::MoveTabNext
            | ActionId::CloseTab
            | ActionId::RenamePane
            | ActionId::ClearPaneName
            | ActionId::SwapWithFocusedPane
            | ActionId::EditScrollback
            | ActionId::CopyMode
            | ActionId::SplitRight
            | ActionId::SplitDown
            | ActionId::ClosePane
            | ActionId::Zoom
            | ActionId::RightClickPassthrough
            | ActionId::EnterResizeMode
            | ActionId::FocusPaneLeft
            | ActionId::FocusPaneDown
            | ActionId::FocusPaneUp
            | ActionId::FocusPaneRight
            | ActionId::CyclePaneNext
            | ActionId::CyclePanePrevious
            | ActionId::LastPane
            | ActionId::LinkHints
            | ActionId::ToggleSidebar
            | ActionId::Detach
            | ActionId::CustomCommand
            | ActionId::MachineImport
            | ActionId::Snippets
            | ActionId::SnippetRun
            | ActionId::SceneSave
            | ActionId::SceneRestore
            | ActionId::Broadcast
            | ActionId::MachineConnect
            | ActionId::MachineSwitch
            | ActionId::MachineRename
            | ActionId::MachineEdit
            | ActionId::MachineEnabled
            | ActionId::MachineCopyFixCommand
            | ActionId::MachineRemove
            | ActionId::Monitor
            | ActionId::CloseMonitor
            | ActionId::ArrangeLayout
            | ActionId::LockLayout
            | ActionId::AgentFocus
            | ActionId::AgentViewActivity
            | ActionId::AgentRename
            | ActionId::AgentUsage
            | ActionId::AgentBindAccount
            | ActionId::AgentClose => ALL_IDS.contains(&id),
        }
    }

    #[test]
    fn every_action_id_has_exactly_one_table_entry_and_a_unique_key() {
        for id in ALL_IDS {
            assert!(listed(*id));
            let count = ACTIONS.iter().filter(|spec| spec.id == *id).count();
            assert_eq!(count, 1, "{id:?} 应在动作表里恰好登记一次");
            assert_eq!(action_spec(*id).id, *id);
        }
        assert_eq!(ACTIONS.len(), ALL_IDS.len(), "动作表里不应有多余条目");
        let mut keys = ACTIONS.iter().map(|spec| spec.key).collect::<Vec<_>>();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), ACTIONS.len(), "稳定 id 不得重复");
    }

    #[test]
    fn every_action_has_titles_and_labels_in_every_language() {
        for texts in [&crate::i18n::en::TEXTS, &crate::i18n::zh_cn::TEXTS] {
            for spec in ACTIONS {
                for alternate in [false, true] {
                    assert!(
                        !spec.title_text(texts, alternate).trim().is_empty(),
                        "{:?} 缺标题",
                        spec.id
                    );
                    assert!(
                        !spec.label_text(texts, alternate).trim().is_empty(),
                        "{:?} 缺菜单标签",
                        spec.id
                    );
                }
                if spec.palette == PaletteMode::PerMachine {
                    assert!(
                        spec.title_text(texts, false).contains("{label}"),
                        "{:?} 按机器铺开的标题必须带 {{label}}",
                        spec.id
                    );
                }
            }
        }
    }

    #[test]
    fn context_identities_are_unique_per_object_kind() {
        let mut seen = Vec::new();
        for spec in ACTIONS {
            if let Some(slot) = spec.context {
                assert!(!seen.contains(&slot), "{slot:?} 重复登记");
                seen.push(slot);
                assert_eq!(context_action(slot.0, slot.1), Some(spec.id));
            }
        }
    }

    #[test]
    fn same_category_groups_are_contiguous_in_declaration_order() {
        // 主菜单子菜单按声明顺序列出、在组号变化处画分隔线：同组条目被别的组
        // 隔开就会画出两段重复的组。
        for category in 0..7 {
            let groups = ACTIONS
                .iter()
                .filter(|spec| spec.category.index() == category)
                .filter(|spec| spec.palette != PaletteMode::Hidden)
                .map(|spec| spec.group)
                .collect::<Vec<_>>();
            let mut closed = Vec::new();
            for window in groups.windows(2) {
                if window[0] != window[1] {
                    closed.push(window[0]);
                    assert!(
                        !closed.contains(&window[1]),
                        "分类 {category} 的组 {} 不相邻：{groups:?}",
                        window[1]
                    );
                }
            }
        }
    }

    #[test]
    fn global_state_hides_layout_actions_outside_the_workbench() {
        let classic = GlobalActionContext::default();
        for id in [
            ActionId::ArrangeLayout,
            ActionId::LockLayout,
            ActionId::CloseMonitor,
        ] {
            assert!(!global_action_state(id, &classic).visible, "{id:?}");
        }
        let workbench = GlobalActionContext {
            workbench: true,
            layout_locked: true,
            ..GlobalActionContext::default()
        };
        assert_eq!(
            global_action_state(ActionId::LockLayout, &workbench).checked,
            Some(true)
        );
        let close = global_action_state(ActionId::CloseMonitor, &workbench);
        assert!(close.visible && !close.enabled, "没停靠监控面板时置灰");
    }

    #[test]
    fn machine_group_state_tracks_profile_and_connection() {
        let offline = machine_action_state(ActionId::MachineConnect, true, false, false);
        assert!(offline.enabled);
        assert!(!machine_action_state(ActionId::MachineConnect, true, true, false).enabled);
        assert!(!machine_action_state(ActionId::MachineConnect, false, false, false).enabled);
        assert!(machine_action_state(ActionId::MachineSwitch, true, true, false).enabled);
        assert!(!machine_action_state(ActionId::MachineSwitch, true, true, true).enabled);
        assert_eq!(
            machine_action_state(ActionId::MachineEnabled, false, false, false).checked,
            Some(false)
        );
    }
}
