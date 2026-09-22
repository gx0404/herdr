//! 菜单信息架构：命令面板、主菜单、右键菜单与 mobile 菜单都从同一张动作表取数。

use super::super::action_table::{context_action, ActionId, ContextKind, PaletteMode, ACTIONS};
use super::super::command_palette::{palette_rows, ClientPaletteAction};
use super::*;
use crate::client::endpoint::{
    ClientEndpointId, ClientEndpointStatus, ProfileId, SavedSshEndpoint,
};

fn profile(label: &str, seed: char) -> SavedSshEndpoint {
    let hex = std::iter::repeat_n(seed, 32).collect::<String>();
    SavedSshEndpoint {
        id: ProfileId::parse(hex).expect("hex profile id"),
        label: label.into(),
        target: format!("{label}@example.com"),
        ..SavedSshEndpoint::new(label, "example.com", "default").expect("valid profile")
    }
}

fn state_with_profiles(profiles: &[SavedSshEndpoint]) -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_endpoint_catalog(profiles);
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state
}

/// 整帧文本，去掉空白（宽字符的续格是空格，按字符断言时不关心列位）。
fn compact_text(frame: &FrameData) -> String {
    frame_rows(frame)
        .join("\n")
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect()
}

fn compact(text: &str) -> String {
    text.chars().filter(|ch| !ch.is_whitespace()).collect()
}

/// 命令面板里的每个动作条目都能在动作表里找到出处，分类与表里一致；表里对
/// 当前布局可见的「聚焦对象」动作一条不落。
#[test]
fn palette_items_are_expanded_from_the_action_table() {
    let prod = profile("prod", 'a');
    let mut state = state_with_profiles(std::slice::from_ref(&prod));
    state.open_command_search();
    let palette = palette_overlay(&state);
    for item in &palette.items {
        let ClientPaletteAction::Run(id, _) = &item.action else {
            continue;
        };
        let spec = ACTIONS
            .iter()
            .find(|spec| spec.id == *id)
            .expect("动作表里有这个动作");
        let expected_key = match spec.palette {
            PaletteMode::Focused => item.id == spec.key,
            PaletteMode::PerMachine | PaletteMode::PerCommand => {
                item.id.starts_with(&format!("{}:", spec.key))
            }
            PaletteMode::Hidden => false,
        };
        assert!(expected_key, "{} 不是 {:?} 的稳定 id", item.id, spec.id);
        assert_eq!(item.category, spec.category.index(), "{}", item.id);
    }
    for spec in ACTIONS
        .iter()
        .filter(|spec| spec.palette == PaletteMode::Focused)
    {
        let listed = palette.items.iter().any(|item| item.id == spec.key);
        let hidden_here = matches!(
            spec.id,
            // 经典布局下不适用；没有更新与发行说明时不列。
            ActionId::CloseMonitor
                | ActionId::ArrangeLayout
                | ActionId::LockLayout
                | ActionId::WhatsNew
        );
        assert_eq!(listed, !hidden_here, "{:?}", spec.id);
    }
}

/// 机器动作三处合并：命令面板按机器铺开的一组、机器行右键菜单的一组，落到同一批
/// 动作表条目；开关类用勾选态而不是「启用 / 停用」两套文案。
#[test]
fn machine_actions_are_one_group_for_palette_and_context_menu() {
    let prod = profile("prod", 'a');
    let prod_id = ClientEndpointId::Ssh(prod.id.clone());
    let mut state = state_with_profiles(std::slice::from_ref(&prod));
    state.open_command_search();
    let machine_ids = palette_overlay(&state)
        .items
        .iter()
        .filter_map(|item| match &item.action {
            ClientPaletteAction::Run(id, _) if item.id.ends_with(prod.id.as_str()) => Some(*id),
            _ => None,
        })
        .collect::<Vec<_>>();
    let group = ACTIONS
        .iter()
        .filter(|spec| spec.palette == PaletteMode::PerMachine)
        .map(|spec| spec.id)
        .collect::<Vec<_>>();
    assert_eq!(machine_ids, group, "每台机器一整组、按表序");
    let toggle = palette_overlay(&state)
        .items
        .iter()
        .find(|item| item.id == format!("machine:toggle:{}", prod.id.as_str()))
        .expect("启用开关");
    assert_eq!(toggle.checked, Some(true));
    assert!(toggle.title.contains("prod"));

    // 机器行右键菜单的每一项（管理机器除外）都是同一组里的条目。
    state.overlay = None;
    state.open_machine_context_menu(&prod_id, 4, 4);
    let Some(ClientShellOverlay::ContextMenu(menu)) = state.overlay.as_ref() else {
        panic!("machine context menu");
    };
    for item in menu.items() {
        let id = context_action(ContextKind::Machine, item.action).expect("动作表条目");
        assert!(
            id == ActionId::ManageMachines || group.contains(&id),
            "{id:?} 不在机器动作组里"
        );
    }

    // 两个入口执行同一个分支：复制修复命令的产物逐字相同。
    let copy_from_menu = {
        let index = menu
            .items()
            .iter()
            .position(|item| item.action == ClientContextMenuAction::CopyMachineFixCommand)
            .expect("复制修复命令");
        let mut outcome = ClientShellInput::default();
        state.activate_context_menu_item(index, &mut outcome);
        outcome.actions
    };
    state.open_command_search();
    palette_select(
        &mut state,
        &format!("machine:copy-fix:{}", prod.id.as_str()),
    );
    let copy_from_palette = state.handle_input_bytes(b"\r").actions;
    let clipboard = |actions: &[ClientShellAction]| {
        actions
            .iter()
            .find_map(|action| match action {
                ClientShellAction::ClipboardWrite(bytes) => Some(bytes.clone()),
                _ => None,
            })
            .expect("clipboard write")
    };
    assert_eq!(clipboard(&copy_from_menu), clipboard(&copy_from_palette));
}

/// 暂时不可用的机器动作：目录视图照样列出（置灰），搜索不列、激活是空操作。
#[test]
fn unavailable_actions_are_listed_in_menus_but_not_in_search() {
    let prod = profile("prod", 'b');
    let prod_id = ClientEndpointId::Ssh(prod.id.clone());
    let mut state = state_with_profiles(std::slice::from_ref(&prod));
    state.set_endpoint_status(&prod_id, ClientEndpointStatus::Online);
    state.cache_endpoint_snapshot(&prod_id, Box::new(snapshot()));
    let connect_id = format!("machine:connect:{}", prod.id.as_str());

    state.open_command_search();
    assert!(state.insert_overlay_text("prod"));
    let rows = palette_rows(palette_overlay(&state));
    assert!(
        rows.iter().all(|row| row.item.id != connect_id),
        "已在线的机器不提供「连接」"
    );

    state.toggle_global_menu();
    state.toggle_global_menu();
    let machines = palette_row_index(&state, "category:2");
    state.activate_palette_item(machines, &mut ClientShellInput::default());
    let rows = palette_rows(palette_overlay(&state));
    let connect = rows
        .iter()
        .position(|row| row.item.id == connect_id)
        .expect("目录视图照样列出");
    assert!(!rows[connect].item.enabled);
    let mut outcome = ClientShellInput::default();
    state.activate_palette_item(connect, &mut outcome);
    assert!(outcome.actions.is_empty(), "不可用条目激活是空操作");
    assert!(
        matches!(state.overlay, Some(ClientShellOverlay::CommandPalette(_))),
        "菜单保持打开"
    );
}

/// mobile 菜单是动作表的渲染层：条目、顺序与标签都来自表；暂时不可用的条目照样
/// 列出、标注后缀，点击不生效。
#[test]
fn mobile_menu_rows_match_the_action_table() {
    let mut projected = snapshot();
    projected.latest_release_notes_available = true;
    projected.release_notes = None;
    let entries = super::super::global_menu::global_menu_items(&projected);
    let expected = ACTIONS
        .iter()
        .filter(|spec| spec.mobile)
        .map(|spec| spec.id)
        .collect::<Vec<_>>();
    assert_eq!(
        entries.iter().map(|entry| entry.id).collect::<Vec<_>>(),
        expected,
        "有发行说明时 mobile 菜单列出全部 mobile 条目"
    );
    let whats_new = entries
        .iter()
        .find(|entry| entry.id == ActionId::WhatsNew)
        .expect("更新内容");
    assert!(!whats_new.enabled, "正文没到时不可用");
    let without_notes = super::super::global_menu::global_menu_items(&snapshot());
    assert!(
        without_notes
            .iter()
            .all(|entry| entry.id != ActionId::WhatsNew),
        "没有更新与发行说明时不列"
    );

    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface());
    state.mode = ClientShellMode::Navigate;
    let frame = state.compose(44, 30).expect("mobile switcher");
    let text = compact_text(&frame);
    let t = crate::i18n::texts();
    let suffixed = compact(&format!("{}{}", whats_new.label, t.menu.unavailable_suffix));
    assert!(text.contains(&suffixed), "不可用条目带后缀：{text}");
    for entry in entries.iter().filter(|entry| entry.enabled) {
        assert!(
            text.contains(&compact(entry.label)),
            "缺少 {}：{text}",
            entry.label
        );
    }
}
