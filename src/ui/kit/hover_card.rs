//! 悬停卡：统一 link / chrome / observability 三套 hover 的计时（停留多久出现、
//! 离开后宽限多久消失、钉住后不随指针离开）与定位（锚点下方左对齐 → 放不下上翻
//! → 两侧都放不下取大侧收缩；永不盖住锚点）。只管状态机与几何，卡片内容由调用方
//! 画；状态放在调用方自己的 `Option<HoverState<T>>` 里，时间由调用方传入（可测）。

use std::time::{Duration, Instant};

use ratatui::layout::Rect;

/// 计时参数。默认值与监控面板既有口径一致：停留 400 ms 出现
/// （`usage.hover_delay_ms` 默认值），离开后宽限 250 ms 消失。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HoverTiming {
    pub show_after: Duration,
    pub leave_grace: Duration,
}

impl Default for HoverTiming {
    fn default() -> Self {
        Self {
            show_after: Duration::from_millis(400),
            leave_grace: Duration::from_millis(250),
        }
    }
}

/// 一张悬停卡的状态（字段对齐 `client/shell/observability.rs::Hover`）。
/// `since` = 指针进入当前目标的时刻；`leave_at` = 离开宽限的到期时刻（指针回到
/// 目标或卡片上即撤销）；`pinned` = 钉住，只由 Esc / 点外关闭，不随指针离开。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HoverState<T> {
    pub target: T,
    pub anchor: Rect,
    pub since: Instant,
    pub visible: bool,
    pub leave_at: Option<Instant>,
    pub pinned: bool,
}

impl<T: PartialEq> HoverState<T> {
    /// 指针进入（或仍停在）`target`：同一目标只撤销离开计时并跟随锚点；换目标
    /// （或原来没有卡）则重新计时、先不可见。返回是否需要重绘：换掉一张可见的卡、
    /// 或可见卡的锚点移动了。
    pub(crate) fn enter(slot: &mut Option<Self>, target: T, anchor: Rect, now: Instant) -> bool {
        if let Some(hover) = slot.as_mut().filter(|hover| hover.target == target) {
            hover.leave_at = None;
            let moved = hover.anchor != anchor;
            hover.anchor = anchor;
            return moved && hover.visible;
        }
        let was_visible = slot.as_ref().is_some_and(|hover| hover.visible);
        *slot = Some(Self {
            target,
            anchor,
            since: now,
            visible: false,
            leave_at: None,
            pinned: false,
        });
        was_visible
    }

    /// 指针离开所有目标（也不在卡片上）：未钉住的卡记下离开时刻；已在计时的不
    /// 推迟（来回抖动不会无限续命）。没有卡或已钉住时什么都不做。
    pub(crate) fn leave(slot: &mut Option<Self>, now: Instant, timing: &HoverTiming) {
        if let Some(hover) = slot.as_mut().filter(|hover| !hover.pinned) {
            hover
                .leave_at
                .get_or_insert_with(|| now.checked_add(timing.leave_grace).unwrap_or(now));
        }
    }

    /// 推进计时：离开宽限到期（且未钉住）→ 清空 `slot`；停留满 `show_after` 且
    /// 指针还没离开 → 变可见。返回是否需要重绘（出现，或擦掉一张可见的卡）。
    pub(crate) fn tick(slot: &mut Option<Self>, now: Instant, timing: &HoverTiming) -> bool {
        let Some(hover) = slot.as_mut() else {
            return false;
        };
        if !hover.pinned && hover.leave_at.is_some_and(|at| now >= at) {
            let was_visible = hover.visible;
            *slot = None;
            return was_visible;
        }
        if !hover.visible
            && hover.leave_at.is_none()
            && now.saturating_duration_since(hover.since) >= timing.show_after
        {
            hover.visible = true;
            return true;
        }
        false
    }
}

impl<T> HoverState<T> {
    /// 指针移到卡片自身上：撤销离开计时（卡片可交互，移过去时不能消失）。
    pub(crate) fn hold(&mut self) {
        self.leave_at = None;
    }

    /// 下一次需要 [`HoverState::tick`] 的时刻：离开宽限的到期时刻，或即将出现的
    /// 时刻；都没有（已可见且指针在上、或已钉住）时为 `None`。供事件循环定超时，
    /// 不必固定频率轮询。
    pub(crate) fn next_deadline(&self, timing: &HoverTiming) -> Option<Instant> {
        if let Some(at) = self.leave_at.filter(|_| !self.pinned) {
            return Some(at);
        }
        if self.visible || self.leave_at.is_some() {
            return None;
        }
        Some(
            self.since
                .checked_add(timing.show_after)
                .unwrap_or(self.since),
        )
    }
}

/// 给 `size = (宽, 高)` 的卡找位置：优先锚点正下方、与锚点左对齐（右侧放不下时
/// 向左平移）；下方高度不够就翻到上方；两侧都不够时取空间大的一侧并把高度收缩到
/// 该侧可用高度。卡片与锚点在纵向上永不重叠。宽度超出 `bounds` 时收缩；没有任何
/// 可用空间（或尺寸为零）时返回空 `Rect`。
pub(crate) fn place_hover_card(anchor: Rect, size: (u16, u16), bounds: Rect) -> Rect {
    let (want_w, want_h) = size;
    if want_w == 0 || want_h == 0 || bounds.is_empty() {
        return Rect::default();
    }
    let width = want_w.min(bounds.width);
    let x = anchor.x.clamp(bounds.x, bounds.right() - width);
    let below_top = anchor.bottom().clamp(bounds.y, bounds.bottom());
    let above_bottom = anchor.y.clamp(bounds.y, bounds.bottom());
    let below = bounds.bottom() - below_top;
    let above = above_bottom - bounds.y;
    if want_h <= below {
        Rect::new(x, below_top, width, want_h)
    } else if want_h <= above {
        Rect::new(x, above_bottom - want_h, width, want_h)
    } else if below >= above && below > 0 {
        Rect::new(x, below_top, width, below)
    } else if above > 0 {
        Rect::new(x, bounds.y, width, above)
    } else {
        Rect::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIMING: HoverTiming = HoverTiming {
        show_after: Duration::from_millis(400),
        leave_grace: Duration::from_millis(250),
    };

    fn ms(base: Instant, millis: u64) -> Instant {
        base + Duration::from_millis(millis)
    }

    #[test]
    fn default_timing_matches_the_existing_monitor_hover() {
        assert_eq!(HoverTiming::default(), TIMING);
    }

    #[test]
    fn card_appears_after_the_dwell_time_and_leaves_after_the_grace() {
        let t0 = Instant::now();
        let anchor = Rect::new(2, 3, 10, 1);
        let mut slot: Option<HoverState<u32>> = None;
        assert!(
            !HoverState::enter(&mut slot, 7, anchor, t0),
            "首次进入不可见，无需重绘"
        );
        assert_eq!(
            slot.as_ref().map(|h| h.next_deadline(&TIMING)),
            Some(Some(ms(t0, 400)))
        );
        assert!(!HoverState::tick(&mut slot, ms(t0, 399), &TIMING));
        assert!(
            HoverState::tick(&mut slot, ms(t0, 400), &TIMING),
            "停留满 400 ms 出现"
        );
        assert!(slot.as_ref().is_some_and(|h| h.visible));
        assert_eq!(slot.as_ref().and_then(|h| h.next_deadline(&TIMING)), None);
        assert!(
            !HoverState::tick(&mut slot, ms(t0, 900), &TIMING),
            "可见后不再重绘"
        );

        HoverState::leave(&mut slot, ms(t0, 1000), &TIMING);
        HoverState::leave(&mut slot, ms(t0, 1100), &TIMING);
        assert_eq!(
            slot.as_ref().and_then(|h| h.leave_at),
            Some(ms(t0, 1250)),
            "重复离开不推迟到期时刻"
        );
        assert_eq!(
            slot.as_ref().and_then(|h| h.next_deadline(&TIMING)),
            Some(ms(t0, 1250))
        );
        assert!(!HoverState::tick(&mut slot, ms(t0, 1249), &TIMING));
        assert!(
            HoverState::tick(&mut slot, ms(t0, 1250), &TIMING),
            "擦掉可见的卡"
        );
        assert!(slot.is_none());
        assert!(!HoverState::tick(&mut slot, ms(t0, 2000), &TIMING));
    }

    #[test]
    fn re_entering_cancels_the_leave_and_switching_targets_restarts_the_dwell() {
        let t0 = Instant::now();
        let anchor = Rect::new(0, 0, 4, 1);
        let mut slot = None;
        HoverState::enter(&mut slot, "a", anchor, t0);
        HoverState::tick(&mut slot, ms(t0, 400), &TIMING);
        HoverState::leave(&mut slot, ms(t0, 500), &TIMING);
        assert!(
            !HoverState::enter(&mut slot, "a", anchor, ms(t0, 600)),
            "同目标无需重绘"
        );
        assert_eq!(slot.as_ref().and_then(|h| h.leave_at), None);
        assert!(
            HoverState::enter(&mut slot, "a", Rect::new(0, 1, 4, 1), ms(t0, 650)),
            "可见卡的锚点移动要重绘"
        );
        assert!(
            !HoverState::tick(&mut slot, ms(t0, 900), &TIMING),
            "宽限已撤销"
        );
        assert!(slot.is_some());

        assert!(
            HoverState::enter(&mut slot, "b", anchor, ms(t0, 1000)),
            "换目标要擦掉旧卡"
        );
        let hover = slot.as_ref().expect("hover");
        assert_eq!(hover.target, "b");
        assert!(!hover.visible);
        assert_eq!(hover.since, ms(t0, 1000));
        // 指针离开后、出现之前到了停留时刻：不再出现。
        HoverState::leave(&mut slot, ms(t0, 1100), &TIMING);
        assert!(!HoverState::tick(&mut slot, ms(t0, 1340), &TIMING));
        assert!(slot.as_ref().is_some_and(|h| !h.visible));
        assert!(
            !HoverState::tick(&mut slot, ms(t0, 1400), &TIMING),
            "从未可见，清掉无需重绘"
        );
        assert!(slot.is_none());
    }

    #[test]
    fn pinned_cards_ignore_the_pointer_and_hold_cancels_the_leave() {
        let t0 = Instant::now();
        let mut slot = None;
        HoverState::enter(&mut slot, 1u8, Rect::new(0, 0, 1, 1), t0);
        HoverState::tick(&mut slot, ms(t0, 400), &TIMING);
        if let Some(hover) = slot.as_mut() {
            hover.pinned = true;
        }
        HoverState::leave(&mut slot, ms(t0, 500), &TIMING);
        assert_eq!(slot.as_ref().and_then(|h| h.leave_at), None, "钉住不计离开");
        assert!(!HoverState::tick(&mut slot, ms(t0, 5000), &TIMING));
        assert!(slot.is_some());
        assert_eq!(slot.as_ref().and_then(|h| h.next_deadline(&TIMING)), None);

        if let Some(hover) = slot.as_mut() {
            hover.pinned = false;
        }
        HoverState::leave(&mut slot, ms(t0, 6000), &TIMING);
        if let Some(hover) = slot.as_mut() {
            hover.hold();
        }
        assert!(
            !HoverState::tick(&mut slot, ms(t0, 7000), &TIMING),
            "指针在卡上"
        );
        assert!(slot.is_some());
    }

    #[test]
    fn placement_prefers_below_then_flips_above_then_shrinks() {
        let bounds = Rect::new(0, 0, 40, 20);
        let anchor = Rect::new(5, 4, 10, 1);
        assert_eq!(
            place_hover_card(anchor, (20, 6), bounds),
            Rect::new(5, 5, 20, 6),
            "下方左对齐"
        );
        let low = Rect::new(5, 16, 10, 1);
        assert_eq!(
            place_hover_card(low, (20, 6), bounds),
            Rect::new(5, 10, 20, 6),
            "下方只剩 3 行 → 上翻，底边贴住锚点"
        );
        let middle = Rect::new(5, 9, 10, 1);
        assert_eq!(
            place_hover_card(middle, (20, 15), bounds),
            Rect::new(5, 10, 20, 10),
            "两侧都不够 → 取下方（10 行 > 上方 9 行）并收缩"
        );
        let upper = Rect::new(5, 11, 10, 1);
        assert_eq!(
            place_hover_card(upper, (20, 15), bounds),
            Rect::new(5, 0, 20, 11),
            "上方更大 → 取上方"
        );
        for (anchor, size) in [
            (anchor, (20, 6)),
            (low, (20, 6)),
            (middle, (20, 15)),
            (upper, (20, 15)),
        ] {
            let rect = place_hover_card(anchor, size, bounds);
            assert!(!rect.intersects(anchor), "{rect:?} 盖住了锚点 {anchor:?}");
            assert_eq!(bounds.intersection(rect), rect, "{rect:?} 越出边界");
        }
    }

    #[test]
    fn placement_never_covers_the_anchor_and_clamps_horizontally() {
        let bounds = Rect::new(0, 0, 30, 10);
        let anchor = Rect::new(25, 2, 5, 2);
        let rect = place_hover_card(anchor, (12, 4), bounds);
        assert_eq!(rect, Rect::new(18, 4, 12, 4), "右侧放不下向左平移");
        assert!(!rect.intersects(anchor));
        assert_eq!(
            place_hover_card(anchor, (50, 3), bounds),
            Rect::new(0, 4, 30, 3),
            "宽度收缩到边界"
        );
        let full = Rect::new(0, 0, 30, 10);
        assert_eq!(
            place_hover_card(full, (5, 2), bounds),
            Rect::default(),
            "锚点占满，无处可放"
        );
        assert_eq!(place_hover_card(anchor, (0, 3), bounds), Rect::default());
        assert_eq!(
            place_hover_card(anchor, (3, 3), Rect::default()),
            Rect::default()
        );
        // 锚点在边界之外（上方）：卡片从边界顶端开始。
        let outside = Rect::new(3, 0, 4, 1);
        let inner = Rect::new(0, 5, 30, 10);
        assert_eq!(
            place_hover_card(outside, (6, 3), inner),
            Rect::new(3, 5, 6, 3)
        );
    }
}
