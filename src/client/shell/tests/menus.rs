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

// ---- 右键菜单：kit::menu 渲染、键盘与鼠标 ----

fn key_event(code: crossterm::event::KeyCode) -> RawInputEvent {
    RawInputEvent::Key(crate::input::TerminalKey::new(code, KeyModifiers::empty()))
}

fn mouse_event(kind: MouseEventKind, column: u16, row: u16) -> RawInputEvent {
    RawInputEvent::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::empty(),
    })
}

/// 保留帧缓冲里 `area` 内第 `y` 行的逐格字形（宽字符的续格是空格）。
fn row_cells(state: &ClientShellState, area: Rect, y: u16) -> Vec<String> {
    let buffer = state.compose_buffer.as_ref().expect("保留帧缓冲");
    (area.x..area.right())
        .map(|x| buffer[(x, y)].symbol().to_owned())
        .collect()
}

/// ASCII 串在逐格字形里的起始列（相对 `area.x`）。
fn find_cells(cells: &[String], needle: &str) -> Option<usize> {
    let wanted = needle.chars().map(String::from).collect::<Vec<_>>();
    cells
        .windows(wanted.len())
        .position(|window| window == wanted.as_slice())
}

/// 画着 `label` 的那一行（绝对 y）。
fn menu_row_with(state: &ClientShellState, area: Rect, label: &str) -> u16 {
    (area.y..area.bottom())
        .find(|y| compact(&row_cells(state, area, *y).concat()).contains(&compact(label)))
        .unwrap_or_else(|| panic!("菜单里没有 {label}"))
}

/// `label` 首字符所在的格（绝对坐标）。
fn label_cell(state: &ClientShellState, area: Rect, label: &str) -> (u16, u16) {
    let y = menu_row_with(state, area, label);
    let first = label.chars().next().expect("非空标签").to_string();
    let x = row_cells(state, area, y)
        .iter()
        .position(|cell| *cell == first)
        .expect("标签首字符");
    (area.x + x as u16, y)
}

fn pane_menu_state(right_click_passthrough: bool) -> ClientShellState {
    let mut projected = snapshot();
    projected.panes[0].right_click_passthrough = right_click_passthrough;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface());
    state.compose(106, 30).expect("shell frame");
    state.open_pane_context_menu("pane_1".into(), 40, 3);
    state.compose(106, 30).expect("context menu frame");
    state
}

fn context_menu(state: &ClientShellState) -> &ClientContextMenuOverlay {
    match state.overlay.as_ref() {
        Some(ClientShellOverlay::ContextMenu(menu)) => menu,
        other => panic!("右键菜单应打开: {other:?}"),
    }
}

fn item_index(state: &ClientShellState, action: ClientContextMenuAction) -> usize {
    context_menu(state)
        .items()
        .iter()
        .position(|item| item.action == action)
        .unwrap_or_else(|| panic!("缺少 {action:?}"))
}

/// 窗格右键菜单走 kit::menu：分组之间是与边框相接的分隔线；快捷键取用户当前
/// 键位、右对齐成一列；暂时不可用的条目置灰照常列出；破坏性动作标红；子菜单
/// 父项行尾画箭头。
#[test]
fn pane_context_menu_draws_separators_right_aligned_shortcuts_and_dimmed_rows() {
    let state = pane_menu_state(false);
    let area = state.hits.overlay_bounds;
    assert!(!area.is_empty());
    let glyphs = state.config.border_glyphs;
    let palette = &state.config.palette;
    let t = &crate::i18n::texts().context_menu;

    // 分隔线：左右两端是 T 形接头，中间整行横线。
    let separators = (area.y + 1..area.bottom() - 1)
        .filter(|y| {
            let cells = row_cells(&state, area, *y);
            cells[0] == glyphs.tee_right
                && cells[cells.len() - 1] == glyphs.tee_left
                && cells[1..cells.len() - 1]
                    .iter()
                    .all(|cell| *cell == glyphs.horizontal)
        })
        .count();
    assert_eq!(separators, 3, "重命名 | 分屏与交换 | 视图 | 关闭 四组");

    // 快捷键：取当前键位的标签，所有快捷键的末列对齐。
    let keybinds = &state.config.keybinds.keybinds;
    let mut ends = Vec::new();
    for (label, keys) in [
        (t.rename_pane, &keybinds.rename_pane),
        (t.split_right, &keybinds.split_vertical),
        (t.split_down, &keybinds.split_horizontal),
        (t.close_pane, &keybinds.close_pane),
    ] {
        let shortcut = keys.labels().into_iter().next().expect("默认键位");
        let y = menu_row_with(&state, area, label);
        let cells = row_cells(&state, area, y);
        let start = find_cells(&cells, &shortcut)
            .unwrap_or_else(|| panic!("{label} 行应带快捷键 {shortcut}: {cells:?}"));
        ends.push(start + shortcut.chars().count());
    }
    assert!(
        ends.windows(2).all(|pair| pair[0] == pair[1]),
        "快捷键右对齐：{ends:?}"
    );

    // 置灰：没有手动名字的「清除窗格名称」与没有来源窗格的「交换」照常列出。
    for label in [t.clear_pane_name, t.swap_with_focused] {
        let (x, y) = label_cell(&state, area, label);
        let cell = &state.compose_buffer.as_ref().expect("帧缓冲")[(x, y)];
        assert_eq!(cell.style().fg, Some(palette.overlay0), "{label} 应置灰");
    }
    // 破坏性动作标红。
    let (x, y) = label_cell(&state, area, t.close_pane);
    let cell = &state.compose_buffer.as_ref().expect("帧缓冲")[(x, y)];
    assert_eq!(cell.style().fg, Some(palette.red));
    // 子菜单父项行尾画箭头。
    let view = crate::i18n::texts().menu.submenu_view;
    let cells = row_cells(&state, area, menu_row_with(&state, area, view));
    assert!(cells.iter().any(|cell| cell == "▸"), "{cells:?}");

    // 禁用项不进命中表：点它不激活，菜单保持打开。
    let clear = item_index(&state, ClientContextMenuAction::ClearPaneName);
    assert!(state
        .hits
        .context_menu_rows
        .iter()
        .all(|(_, index)| *index != clear));
}

/// 二态开关用勾选态：「右键透传给窗格」打开时在子菜单里画勾，而不是换一套
/// 「使用 Herdr 右键菜单」的互斥文案。键盘进出子菜单：→ 进、← 出。
#[test]
fn toggle_items_render_check_marks_in_the_submenu() {
    let t = &crate::i18n::texts().context_menu;
    for passthrough in [true, false] {
        let mut state = pane_menu_state(passthrough);
        // End 落到最后一项（关闭窗格），↑ 回到子菜单父项，→ 打开子菜单。
        state.handle_raw_events(vec![
            key_event(crossterm::event::KeyCode::End),
            key_event(crossterm::event::KeyCode::Up),
            key_event(crossterm::event::KeyCode::Right),
        ]);
        state.compose(106, 30).expect("submenu frame");
        let menu = context_menu(&state);
        let submenu = menu.submenu.as_ref().expect("子菜单已展开");
        assert_eq!(
            submenu.highlighted,
            item_index(&state, ClientContextMenuAction::Zoom),
            "键盘打开子菜单时高亮第一个子项"
        );
        let area = state.hits.overlay_bounds;
        let y = menu_row_with(&state, area, t.send_right_clicks);
        let checked = row_cells(&state, area, y).iter().any(|cell| cell == "✓");
        assert_eq!(checked, passthrough, "勾选态跟随透传开关");
        assert!(
            !compact(
                &(area.y..area.bottom())
                    .map(|y| row_cells(&state, area, y).concat())
                    .collect::<String>()
            )
            .contains(&compact(crate::i18n::en::TEXTS.context_menu.close_group)),
            "不再出现互斥文案"
        );
        state.handle_raw_events(vec![key_event(crossterm::event::KeyCode::Left)]);
        assert!(context_menu(&state).submenu.is_none(), "← 收起子菜单");
    }
}

/// 键盘导航跳过分隔线与禁用项并回绕；Home / End 到首尾；可打印字符按首字母
/// 跳转，连按在同首字母的条目间轮转；回车激活高亮项。
#[test]
fn context_menu_keyboard_navigation_skips_disabled_rows_and_jumps_by_letter() {
    use crossterm::event::KeyCode;
    let mut state = pane_menu_state(false);
    let highlighted = |state: &ClientShellState| context_menu(state).highlighted;
    let rename = item_index(&state, ClientContextMenuAction::RenamePane);
    let split_right = item_index(&state, ClientContextMenuAction::SplitRight);
    let split_down = item_index(&state, ClientContextMenuAction::SplitDown);
    let close = item_index(&state, ClientContextMenuAction::ClosePane);
    assert_eq!(highlighted(&state), rename);

    // ↓ 跳过置灰的「清除窗格名称」与分隔线。
    state.handle_raw_events(vec![key_event(KeyCode::Down)]);
    assert_eq!(highlighted(&state), split_right);
    state.handle_raw_events(vec![key_event(KeyCode::End)]);
    assert_eq!(highlighted(&state), close);
    state.handle_raw_events(vec![key_event(KeyCode::Home)]);
    assert_eq!(highlighted(&state), rename);
    // ↑ 从首项回绕到末项。
    state.handle_raw_events(vec![key_event(KeyCode::Up)]);
    assert_eq!(highlighted(&state), close);

    // 首字母：与「向右分割」同首字母、可用的顶层条目依次轮转。
    let items = context_menu(&state).items();
    let first = items[split_right]
        .label
        .chars()
        .next()
        .expect("非空标签")
        .to_lowercase()
        .next()
        .expect("小写");
    let expected = [rename, split_right, split_down, close]
        .into_iter()
        .filter(|index| {
            items[*index]
                .label
                .chars()
                .next()
                .and_then(|ch| ch.to_lowercase().next())
                == Some(first)
        })
        .collect::<Vec<_>>();
    assert!(expected.len() >= 2, "夹具前提：两个分屏项同首字母");
    let mut visited = Vec::new();
    for _ in 0..expected.len() + 1 {
        state.handle_raw_events(vec![key_event(KeyCode::Char(first))]);
        visited.push(highlighted(&state));
    }
    assert_eq!(&visited[..expected.len()], expected.as_slice());
    assert_eq!(visited[expected.len()], expected[0], "连按回绕");

    // 回车激活高亮项：分屏走端点 API。
    while highlighted(&state) != split_down {
        state.handle_raw_events(vec![key_event(KeyCode::Down)]);
    }
    let outcome = state.handle_raw_events(vec![key_event(KeyCode::Enter)]);
    assert!(state.overlay.is_none());
    assert!(outcome.actions.iter().any(|action| matches!(
        action,
        ClientShellAction::Endpoint { request, .. }
            if matches!(
                &request.method,
                crate::api::schema::Method::PaneSplit(params)
                    if params.direction == crate::api::schema::SplitDirection::Down
            )
    )));
}

/// 指针：悬浮行用主题的 `hover_bg`（与键盘高亮分离）；悬到子菜单父项上展开
/// 子菜单，点子项执行对应动作。
#[test]
fn context_menu_hover_uses_theme_hover_bg_and_opens_the_submenu() {
    let mut state = pane_menu_state(false);
    let split_down = item_index(&state, ClientContextMenuAction::SplitDown);
    let row = state
        .hits
        .context_menu_rows
        .iter()
        .find(|(_, index)| *index == split_down)
        .expect("分屏行")
        .0;
    state.handle_raw_events(vec![mouse_event(MouseEventKind::Moved, row.x + 2, row.y)]);
    state.compose(106, 30).expect("hover frame");
    assert_eq!(context_menu(&state).hovered, Some(split_down));
    assert_eq!(
        context_menu(&state).highlighted,
        item_index(&state, ClientContextMenuAction::RenamePane),
        "悬浮不改键盘高亮"
    );
    // 行首、行尾的内边距格（宽字符的续格不带样式，不取它们）。
    let buffer = state.compose_buffer.as_ref().expect("帧缓冲");
    for x in [row.x, row.right() - 1] {
        assert_eq!(
            buffer[(x, row.y)].style().bg,
            Some(state.config.components.hover_bg)
        );
    }

    let parent = state
        .hits
        .context_menu_rows
        .iter()
        .find(|(_, index)| *index == super::super::context_menu::SUBMENU_ROW)
        .expect("子菜单父项行")
        .0;
    state.handle_raw_events(vec![mouse_event(
        MouseEventKind::Moved,
        parent.x + 2,
        parent.y,
    )]);
    state.compose(106, 30).expect("submenu frame");
    let submenu = context_menu(&state)
        .submenu
        .as_ref()
        .expect("悬浮展开子菜单");
    assert_eq!(
        submenu.highlighted,
        usize::MAX,
        "悬浮展开时键盘焦点留在顶层"
    );
    let zoom = item_index(&state, ClientContextMenuAction::Zoom);
    let zoom_row = state
        .hits
        .context_menu_rows
        .iter()
        .find(|(_, index)| *index == zoom)
        .expect("子菜单行进命中表")
        .0;
    // 子菜单贴在父菜单旁边、与父项同行起：右侧放不下（本例）就翻到左侧，
    // 两层互不遮挡。
    assert_eq!(zoom_row.y, parent.y);
    assert!(
        zoom_row.x > parent.right() || zoom_row.right() < parent.x,
        "子菜单不压父菜单：{zoom_row:?} / {parent:?}"
    );
    let outcome = state.handle_raw_events(vec![mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        zoom_row.x + 1,
        zoom_row.y,
    )]);
    assert!(state.overlay.is_none());
    assert!(outcome.actions.iter().any(|action| matches!(
        action,
        ClientShellAction::Endpoint { request, .. }
            if matches!(&request.method, crate::api::schema::Method::PaneZoom(_))
    )));
}

// ---- 命令面板目录视图：kit::menu ----

fn open_catalog(state: &mut ClientShellState, cols: u16, rows: u16) {
    state.compose(cols, rows).expect("shell frame");
    state.toggle_global_menu();
    state.compose(cols, rows).expect("catalog frame");
}

/// 目录视图 kit 菜单所占的矩形（命中表里的 `menu_popup`）。
fn catalog_area(state: &ClientShellState) -> Rect {
    let area = state.hits.menu_popup;
    assert!(!area.is_empty(), "目录视图应画成菜单");
    area
}

fn catalog_text(state: &ClientShellState) -> Vec<String> {
    let area = catalog_area(state);
    (area.y..area.bottom())
        .map(|y| row_cells(state, area, y).concat())
        .collect()
}

/// 主菜单：分类是子菜单项（行尾 ▸），有更新的分类带徽标；「搜索命令」与分类
/// 之间有分隔线，右列是它的快捷键。
#[test]
fn catalog_main_menu_draws_submenu_arrows_badges_and_the_search_shortcut() {
    let mut projected = snapshot();
    projected.integration_updates_available = true;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface());
    open_catalog(&mut state, 106, 30);
    let t = &crate::i18n::texts().global_menu;
    let area = catalog_area(&state);
    for category in t.categories {
        let y = menu_row_with(&state, area, category);
        let cells = row_cells(&state, area, y);
        assert!(
            cells.iter().any(|cell| cell == "▸"),
            "{category}: {cells:?}"
        );
    }
    // 设置分类里有集成更新：分类行带 ● 徽标。
    let settings = menu_row_with(&state, area, t.categories[5]);
    assert!(row_cells(&state, area, settings)
        .iter()
        .any(|cell| cell == "●"));
    let search = menu_row_with(&state, area, t.command_search);
    let shortcut = state
        .config
        .keybinds
        .keybinds
        .command_search
        .label()
        .expect("默认键位");
    assert!(find_cells(&row_cells(&state, area, search), &shortcut).is_some());
    let glyphs = state.config.border_glyphs;
    assert_eq!(
        row_cells(&state, area, search - 1)[0],
        glyphs.tee_right,
        "搜索命令之前有分隔线"
    );
    // 悬浮底色取主题 hover_bg。
    let (row, index) = state.hits.global_menu_rows[1];
    state.handle_raw_events(vec![mouse_event(MouseEventKind::Moved, row.x, row.y)]);
    state.compose(106, 30).expect("hover frame");
    assert_eq!(palette_overlay(&state).hovered, Some(index));
    let buffer = state.compose_buffer.as_ref().expect("帧缓冲");
    assert_eq!(
        buffer[(row.x, row.y)].style().bg,
        Some(state.config.components.hover_bg)
    );
}

/// 分类子菜单：分类名作标题，动作表的组号变化处画分隔线，「返回」前也有一条；
/// 开关类动作画勾选、暂时不可用的置灰且键盘跳过。→ 进分类、← 回主菜单并选中
/// 刚才的分类。
#[test]
fn catalog_category_submenu_groups_items_and_navigates_with_arrows() {
    use crossterm::event::KeyCode;
    let prod = profile("prod", 'c');
    let prod_id = ClientEndpointId::Ssh(prod.id.clone());
    let mut state = state_with_profiles(std::slice::from_ref(&prod));
    state.set_endpoint_status(&prod_id, ClientEndpointStatus::Online);
    state.cache_endpoint_snapshot(&prod_id, Box::new(snapshot()));
    open_catalog(&mut state, 106, 60);

    // 键盘选到「机器与 SSH」再按 →。
    while palette_rows(palette_overlay(&state))[palette_overlay(&state).selected]
        .item
        .id
        != "category:2"
    {
        state.handle_raw_events(vec![key_event(KeyCode::Down)]);
    }
    state.handle_raw_events(vec![key_event(KeyCode::Right)]);
    assert_eq!(
        palette_overlay(&state).view,
        super::super::command_palette::BrowserView::Menu(Some(2))
    );
    state.compose(106, 60).expect("machines submenu");
    let t = crate::i18n::texts();
    let text = catalog_text(&state);
    assert!(
        compact(&text[1]).contains(&compact(t.global_menu.categories[2])),
        "{text:?}"
    );
    let glyphs = state.config.border_glyphs;
    let separators = text
        .iter()
        .filter(|row| row.starts_with(glyphs.tee_right))
        .count();
    assert_eq!(separators, 2, "管理组 | 每台机器一组 | 返回：{text:?}");
    assert!(
        text.iter().any(|row| row.contains(&prod.target)),
        "机器地址作组标题：{text:?}"
    );

    // 「启用」是勾选项；已在线的机器「连接」置灰、键盘跳过。
    let area = catalog_area(&state);
    let enable = crate::i18n::fill(t.global_menu.machine_enable_fmt, &[("label", "prod")]);
    let enable_y = menu_row_with(&state, area, &enable);
    assert!(row_cells(&state, area, enable_y)
        .iter()
        .any(|cell| cell == "✓"));
    let connect = crate::i18n::fill(t.global_menu.machine_connect_fmt, &[("label", "prod")]);
    let (x, y) = label_cell(&state, area, &connect);
    let buffer = state.compose_buffer.as_ref().expect("帧缓冲");
    assert_eq!(
        buffer[(x, y)].style().fg,
        Some(state.config.palette.overlay0)
    );
    let connect_id = format!("machine:connect:{}", prod.id.as_str());
    for _ in 0..palette_rows(palette_overlay(&state)).len() * 2 {
        state.handle_raw_events(vec![key_event(KeyCode::Down)]);
        let palette = palette_overlay(&state);
        assert_ne!(
            palette_rows(palette)[palette.selected].item.id,
            connect_id,
            "置灰项不可选"
        );
    }

    // ← 回主菜单，选中刚才进入的分类。
    state.handle_raw_events(vec![key_event(KeyCode::Left)]);
    let palette = palette_overlay(&state);
    assert_eq!(
        palette.view,
        super::super::command_palette::BrowserView::Menu(None)
    );
    assert_eq!(
        palette_rows(palette)[palette.selected].item.id,
        "category:2"
    );
    // Home / End 到首尾可用项。
    state.handle_raw_events(vec![key_event(KeyCode::End)]);
    let palette = palette_overlay(&state);
    assert_eq!(palette_rows(palette)[palette.selected].item.id, "search");
    state.handle_raw_events(vec![key_event(KeyCode::Home)]);
    assert_eq!(palette_overlay(&state).selected, 0);
}

/// 行数放不下时按滚动窗口画：键盘高亮永远画在窗口里（视图计算阶段把 `scroll`
/// 写回状态），上 / 下还有项时在边框正中画 ▲ / ▼；滚轮滚走窗口时不拉回高亮。
#[test]
fn catalog_scroll_follows_the_keyboard_highlight() {
    use crossterm::event::KeyCode;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.compose(100, 16).expect("shell frame");
    state.toggle_global_menu();
    let tabs = palette_row_index(&state, "category:1");
    state.activate_palette_item(tabs, &mut ClientShellInput::default());
    let rows = palette_rows(palette_overlay(&state)).len();
    let mut last_scroll = 0;
    let mut scrolled = false;
    for _ in 0..rows {
        state.compose(100, 16).expect("catalog frame");
        let palette = palette_overlay(&state);
        let selected = palette.selected;
        assert!(
            state
                .hits
                .global_menu_rows
                .iter()
                .any(|(_, index)| *index == selected),
            "高亮行 {selected} 必须画在窗口里（scroll {}）",
            palette.scroll
        );
        assert!(palette.scroll >= last_scroll || palette.scroll == 0);
        scrolled |= palette.scroll > 0;
        last_scroll = palette.scroll;
        let area = catalog_area(&state);
        let middle = area.x + area.width / 2;
        let buffer = state.compose_buffer.as_ref().expect("帧缓冲");
        let top = buffer[(middle, area.y)].symbol().to_owned();
        let bottom = buffer[(middle, area.bottom() - 1)].symbol().to_owned();
        assert_eq!(top == "▲", palette.scroll > 0, "上方还有项时画 ▲");
        if palette.selected + 1 < rows {
            assert_eq!(bottom, "▼", "下方还有项时画 ▼");
        }
        state.handle_raw_events(vec![key_event(KeyCode::Down)]);
    }
    assert!(scrolled, "夹具前提：分类条目多于窗口行数");

    // 滚轮：窗口移动而高亮不被拉回。
    state.handle_raw_events(vec![key_event(KeyCode::Home)]);
    state.compose(100, 16).expect("top");
    assert_eq!(palette_overlay(&state).scroll, 0);
    state.scroll_palette(3);
    state.compose(100, 16).expect("wheel");
    let palette = palette_overlay(&state);
    assert_eq!(palette.scroll, 3, "滚轮不把窗口拉回高亮项");
    let selected = palette.selected;
    assert!(state
        .hits
        .global_menu_rows
        .iter()
        .all(|(_, index)| *index != selected));
}

// ---- 入口去重、「«」与「调整布局」 ----

/// 与 `workbench::ready()` 同构：宣告 `client.views.set` 后 tick 一次启用停靠工作台。
fn workbench_state() -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_endpoint_methods(Some(vec!["client.views.set".into(), "tab.focus".into()]));
    state.set_pane_surface(surface());
    state.compose(120, 40).expect("初始画面");
    state.tick_workbench(std::time::Instant::now(), &mut ClientShellInput::default());
    state.workbench.pending = false;
    state.workbench.acknowledged = state.workbench.revision;
    assert!(state.workbench.enabled, "夹具前提：workbench 布局已启用");
    state
}

fn workbench_action_rect(
    state: &ClientShellState,
    wanted: fn(&super::super::workbench::interaction::Action) -> bool,
) -> Rect {
    state
        .workbench
        .hits
        .iter()
        .find(|(_, action)| wanted(action))
        .map(|(rect, _)| *rect)
        .expect("工作台命中区")
}

/// 工作台布局只保留顶栏「herdr ≡」一个主菜单入口：侧栏页脚不画「菜单」，也不画
/// 点不了的「«」；点顶栏入口，目录菜单从入口下沿向下展开。
#[test]
fn workbench_keeps_a_single_menu_entry_in_the_top_bar() {
    let mut state = workbench_state();
    let frame = state.compose(120, 40).expect("workbench frame");
    let rows = frame_rows(&frame);
    let launcher = state.hits.global_launcher;
    assert_eq!(launcher.y, 0, "入口在顶栏：{launcher:?}");
    assert!(compact(&rows[0]).contains("herdr≡"));
    assert!(state.hits.sidebar_toggle.is_empty());
    let menu_label = compact(crate::i18n::texts().sidebar.menu);
    for row in &rows[1..] {
        assert!(
            !compact(row).contains(&menu_label),
            "侧栏页脚不再画菜单：{row}"
        );
        assert!(!row.contains('«'), "工作台下不画「«」：{row}");
    }

    state.handle_raw_events(vec![mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        launcher.x + 1,
        launcher.y,
    )]);
    state.compose(120, 40).expect("catalog frame");
    assert_eq!(
        palette_overlay(&state).view,
        super::super::command_palette::BrowserView::Menu(None)
    );
    let popup = catalog_area(&state);
    assert_eq!(popup.y, launcher.bottom(), "下拉菜单贴着顶栏入口展开");
    assert_eq!(popup.x, launcher.x);
}

/// 经典布局保留页脚「菜单」入口：两个字完整（不被「«」盖掉半格），「«」画在
/// 侧栏右下角且可点；单机侧栏与多机侧栏两条渲染路径都一样。
#[test]
fn classic_footer_keeps_the_menu_label_whole_and_the_toggle_clickable() {
    let prod = profile("prod", 'd');
    let menu_label = crate::i18n::texts().sidebar.menu;
    for mut state in [
        {
            let mut state =
                ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
            state.set_snapshot(Box::new(snapshot()));
            state.set_pane_surface(surface());
            state
        },
        state_with_profiles(std::slice::from_ref(&prod)),
    ] {
        state.compose(106, 30).expect("classic frame");
        let launcher = state.hits.global_launcher;
        let toggle = state.hits.sidebar_toggle;
        assert!(!launcher.is_empty() && !toggle.is_empty());
        assert!(
            launcher.intersection(toggle).is_empty(),
            "入口与「«」不重叠"
        );
        let cells = row_cells(&state, launcher, launcher.y);
        assert_eq!(compact(&cells.concat()), compact(menu_label), "{cells:?}");
        let buffer = state.compose_buffer.as_ref().expect("帧缓冲");
        assert_eq!(buffer[(toggle.x, toggle.y)].symbol(), "«");

        state.handle_raw_events(vec![mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            toggle.x,
            toggle.y,
        )]);
        assert!(state.sidebar_collapsed, "「«」可点：收起侧栏");
    }
}

/// 「菜单」与「«」落在同一行时（侧栏只有工作区一段）菜单槽位让出「«」和一格
/// 间隔。
#[test]
fn footer_menu_slot_steps_aside_for_the_toggle_on_the_same_row() {
    use super::super::endpoint_sidebar::{footer_menu_slot, sidebar_toggle_rect};
    let sidebar = Rect::new(0, 1, 26, 12);
    let workspace = Rect::new(0, 1, 25, 12);
    let footer_y = workspace.bottom() - 1;
    let toggle = sidebar_toggle_rect(sidebar, false);
    assert_eq!(toggle.y, footer_y, "夹具前提：同一行");
    let slot = footer_menu_slot(workspace, footer_y, toggle);
    assert_eq!(slot.right() + 1, toggle.x);
    // 不同行时照常贴右。
    let apart = footer_menu_slot(workspace, footer_y - 3, toggle);
    assert_eq!(apart.right(), workspace.right());
    assert!(
        sidebar_toggle_rect(sidebar, true).is_empty(),
        "工作台不画「«」"
    );
}

/// 顶栏「布局」改名「调整布局」：点它进入模式（按钮反色），页脚换成状态标签 +
/// 键位提示条，「Esc 完成」可点退出；「锁定布局」是勾选态，不再换「解锁」文案。
#[test]
fn arrange_layout_button_enters_the_mode_with_footer_hints() {
    use super::super::workbench::interaction::Action;
    let mut state = workbench_state();
    state.config.mouse_capture = true;
    let t = &crate::i18n::texts().menu;
    let frame = state.compose(120, 40).expect("workbench frame");
    let top = compact(&frame_rows(&frame)[0]);
    assert!(top.contains(&compact(t.arrange_layout)), "{top}");
    assert!(
        top.contains(&compact(t.lock_layout)) && !top.contains('✓'),
        "{top}"
    );

    let arrange = workbench_action_rect(&state, |action| matches!(action, Action::Arrange));
    let mut outcome = ClientShellInput::default();
    assert!(state.workbench_mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: arrange.x + 1,
            row: arrange.y,
            modifiers: KeyModifiers::empty(),
        },
        &mut outcome,
    ));
    assert!(state.workbench.arranging);
    state.workbench.dock.locked = true;
    let frame = state.compose(120, 40).expect("arranging frame");
    let rows = frame_rows(&frame);
    assert!(compact(&rows[0]).contains(&format!("✓{}", compact(t.lock_layout))));
    let footer = compact(&rows[39]);
    for label in [
        t.arrange_hint,
        t.arrange_focus,
        t.arrange_resize,
        t.arrange_maximize,
        t.arrange_done,
    ] {
        assert!(footer.contains(&compact(label)), "页脚缺 {label}：{footer}");
    }
    let buffer = state.compose_buffer.as_ref().expect("帧缓冲");
    let arrange = workbench_action_rect(&state, |action| matches!(action, Action::Arrange));
    assert_eq!(
        buffer[(arrange.x, arrange.y)].style().bg,
        Some(state.config.palette.accent),
        "模式开着时按钮反色"
    );
    // 页脚「Esc 完成」可点：退出调整布局。
    let done = state
        .workbench
        .hits
        .iter()
        .filter(|(_, action)| matches!(action, Action::Arrange))
        .map(|(rect, _)| *rect)
        .find(|rect| rect.y == 39)
        .expect("页脚完成命中区");
    assert!(state.workbench_mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: done.x + 1,
            row: done.y,
            modifiers: KeyModifiers::empty(),
        },
        &mut outcome,
    ));
    assert!(!state.workbench.arranging);
}
