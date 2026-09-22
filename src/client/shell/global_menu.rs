//! mobile 菜单：动作表里标了 `mobile` 的条目的渲染层。条目、标签与可用性都
//! 取自 [`super::action_table`]，这里只负责排成一列并把点击交给
//! [`ClientShellState::run_action`]。

use super::action_table::{
    global_action_state, ActionId, ActionTarget, GlobalActionContext, ACTIONS,
};
use super::*;

/// mobile 菜单的一行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct GlobalMenuEntry {
    pub(super) label: &'static str,
    pub(super) id: ActionId,
    /// 暂时不可用（如发行说明正文还没到）：照样列出，点击不生效。
    pub(super) enabled: bool,
}

pub(super) fn global_menu_attention(snapshot: &ClientShellSnapshot) -> bool {
    snapshot.update_available.is_some() || snapshot.integration_updates_available
}

/// mobile 菜单条目：动作表声明顺序，只取对当前快照可见的。
pub(super) fn global_menu_items(snapshot: &ClientShellSnapshot) -> Vec<GlobalMenuEntry> {
    let texts = crate::i18n::texts();
    let cx = GlobalActionContext::from_snapshot(snapshot);
    ACTIONS
        .iter()
        .filter(|spec| spec.mobile)
        .filter_map(|spec| {
            let state = global_action_state(spec.id, &cx);
            state.visible.then_some(GlobalMenuEntry {
                label: spec.label_text(texts, state.alternate),
                id: spec.id,
                enabled: state.enabled,
            })
        })
        .collect()
}

impl ClientShellState {
    /// 激活 mobile 菜单第 `index` 行；不可用的行是空操作。
    pub(super) fn activate_global_menu_item(
        &mut self,
        index: usize,
        outcome: &mut ClientShellInput,
    ) {
        let Some(entry) = self
            .snapshot
            .as_deref()
            .and_then(|snapshot| global_menu_items(snapshot).get(index).copied())
        else {
            return;
        };
        if !entry.enabled {
            return;
        }
        self.overlay = None;
        self.run_action(entry.id, ActionTarget::Focused, outcome);
        outcome.repaint = true;
    }
}
