//! 浮层、帮助与 which-key 的画廊（Graphite 精修第二阶段）：深浅两套主题，窄屏与宽屏
//! 各一份。`cargo test --bin herdr --locked ui_gallery_overlays -- --ignored` 生成。

use super::*;
use crate::client::shell::feedback::ClientToastLevel;

/// 浮层场景：标题、尺寸、额外配置与打开浮层的动作（在第一帧之后执行）。
type Scene = (&'static str, (u16, u16), Prepare);

fn notifications(state: &mut ClientShellState) {
    state.record_notification(
        ClientToastLevel::Warning,
        "claude 等待输入",
        Some("需要确认是否覆盖 src/client/shell/overlays.rs".into()),
        None,
    );
    state.record_notification(ClientToastLevel::Success, "构建完成", None, None);
    state.record_notification(
        ClientToastLevel::Error,
        "cargo nextest 失败",
        Some("3 项测试失败：which_key / overlays / menus".into()),
        None,
    );
    state.record_notification(ClientToastLevel::Info, "已重载配置", None, None);
    state.open_notification_history();
    if let Some(ClientShellOverlay::NotificationHistory(history)) = state.overlay.as_mut() {
        history.selected = 2;
        history.hovered = Some(0);
    }
}

/// 打开按位置分组的导航器（不是智能体总览视图）。
fn open_navigator(state: &mut ClientShellState) {
    state.open_navigator_overlay();
    if let Some(ClientShellOverlay::Navigator(navigator)) = state.overlay.as_mut() {
        navigator.overview.grouping = crate::client::shell::chrome::overview::Grouping::Location;
    }
}

fn help(state: &mut ClientShellState) {
    state.overlay = Some(ClientShellOverlay::Help(ClientHelpOverlay {
        query: TextEditor::default(),
        search_focused: false,
        scroll: 0,
        max_scroll: 0,
    }));
}

fn scenes() -> Vec<Scene> {
    vec![
        ("help", (80, 24), help),
        ("help", (160, 45), help),
        ("which-key", (60, 20), |state| {
            state.mode = ClientShellMode::Prefix;
        }),
        ("which-key", (120, 40), |state| {
            state.mode = ClientShellMode::Prefix;
        }),
        ("palette custom command", (120, 40), |state| {
            push_custom_command(state);
            state.open_command_search();
            state.handle_input_bytes(b"say");
        }),
        ("palette", (120, 40), |state| state.open_command_search()),
        ("palette no match", (80, 24), |state| {
            state.open_command_search();
            state.handle_input_bytes(b"zzqqxxjj");
        }),
        ("navigator", (160, 45), |state| {
            open_navigator(state);
            let _ = state.compose(160, 45);
            let hovered = state
                .hits
                .navigator_rows
                .iter()
                .rev()
                .find(|(_, target)| matches!(target, ClientNavigatorTarget::Pane { .. }))
                .map(|(_, target)| target.clone());
            if let Some(ClientShellOverlay::Navigator(navigator)) = state.overlay.as_mut() {
                navigator.hovered = hovered;
            }
        }),
        ("navigator stale", (120, 40), |state| {
            state.mark_endpoint_disconnected(&ClientEndpointId::Local);
            open_navigator(state);
        }),
        ("navigator no match", (80, 24), |state| {
            open_navigator(state);
            if let Some(ClientShellOverlay::Navigator(navigator)) = state.overlay.as_mut() {
                navigator.query = "zzzz-no-such-pane".into();
            }
        }),
        ("notifications", (120, 40), notifications),
        ("notifications empty", (80, 24), |state| {
            state.open_notification_history()
        }),
        ("confirm close", (80, 24), |state| {
            state.overlay = Some(ClientShellOverlay::ConfirmClose(
                ClientConfirmCloseOverlay {
                    workspace_id: "w1".into(),
                    close_group: false,
                    tab_target: None,
                    title: "关闭工作区？".into(),
                    detail: "herdr · 3 个窗格，运行中的 agent 会被停止".into(),
                },
            ));
        }),
        ("onboarding", (80, 24), |state| {
            state.overlay = Some(ClientShellOverlay::Onboarding);
        }),
        ("onboarding", (160, 45), |state| {
            state.overlay = Some(ClientShellOverlay::Onboarding);
        }),
        ("rename", (80, 24), |state| {
            state.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
                title: crate::i18n::texts().welcome.path_hint,
                input: TextEditor::new("~/code/herdr", false),
                target: ClientRenameTarget::OpenDirectory,
            }));
        }),
    ]
}

fn config(theme: Option<&str>) -> ClientShellConfig {
    let mut config = Config::default();
    config.theme.name = theme.map(str::to_owned);
    ClientShellConfig::from_config(&config)
}

/// 一条前缀自定义命令（命令清单本由 server 快照给出，画廊直接塞进键位）。
fn push_custom_command(state: &mut ClientShellState) {
    state
        .config
        .keybinds
        .keybinds
        .custom_commands
        .push(crate::config::CustomCommandKeybind {
            bindings: crate::config::ActionKeybinds::prefix("g"),
            label: "prefix+g".into(),
            command: "echo hi".into(),
            action: crate::config::CustomCommandAction::Shell,
            description: Some("say hi from a custom command".into()),
            width: None,
            height: None,
        });
}

#[test]
#[ignore = "manual visual gallery; run `just ui-gallery`"]
fn ui_gallery_overlays() {
    let _lang = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    let mut shots = Vec::new();
    let mut step = 0;
    for (theme, host, tag) in [
        (None, DARK_HOST, "dark"),
        (Some("graphite-light"), LIGHT_HOST, "light"),
    ] {
        for (name, (cols, rows), prepare) in scenes() {
            let mut state = workbench_state(config(theme));
            let _ = shoot_workbench(&mut state, cols, rows, &mut step);
            prepare(&mut state);
            let frame = if state.mode == ClientShellMode::Prefix {
                shoot_workbench(&mut state, cols, rows, &mut step)
            } else {
                state.compose(cols, rows).expect("浮层帧").frame.clone()
            };
            shots.push(Shot {
                title: format!("{tag} {name} {cols}x{rows}"),
                frame,
                host,
            });
        }
    }
    write_shots("overlays", &shots);
}
