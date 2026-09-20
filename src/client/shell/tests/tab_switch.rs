//! 切换标签（计划 2.1）：快照/表面配对闸门与折叠后的 TabFocus 请求。

use super::*;

/// 服务端先推进 projection revision（TabFocus 生效）时，客户端不得用旧表面出帧；
/// 配对表面到达后才组合出帧，且光标跟随焦点窗格可见。「恰一帧」由客户端事件循环
/// 保证（快照到达 → compose None 不出帧；表面到达 → compose Some 出一帧），本用例
/// 只固化 compose 的配对闸门，不断言 compose 的重复调用次数。
#[test]
fn tab_switch_waits_for_the_paired_surface_before_composing_a_frame_with_visible_cursor() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    let baseline = state.compose(106, 20).expect("初始配对帧");
    assert!(baseline
        .cursor
        .as_ref()
        .is_some_and(|cursor| cursor.visible));

    let mut switched = snapshot();
    switched.revision = 2;
    state.set_snapshot(Box::new(switched));
    assert!(
        state.compose(106, 20).is_none(),
        "快照先到、配对表面未到：不出帧（避免用旧表面闪一帧）"
    );
    assert!(
        state.compose(106, 20).is_none(),
        "反复 compose 也不会漏出旧表面"
    );

    let mut paired = surface();
    paired.projection_revision = 2;
    paired.surface_revision = 2;
    state.set_pane_surface(paired);
    let frame = state.compose(106, 20).expect("表面到达后恰好一帧");
    let cursor = frame.cursor.as_ref().expect("焦点窗格光标");
    assert!(cursor.visible, "切换后的帧光标可见");
    assert_eq!(frame.width, 106);
    assert_eq!(frame.height, 20);
}
