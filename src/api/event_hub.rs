use crate::api::schema::{EventEnvelope, EventGap, EventKind};

#[derive(Clone)]
pub struct EventHub {
    inner: std::sync::Arc<std::sync::Mutex<EventHubState>>,
}

impl Default for EventHub {
    fn default() -> Self {
        Self::with_capacity(Self::MAX_EVENTS)
    }
}

struct EventHubState {
    next_sequence: u64,
    /// 环形缓冲容量。生产路径恒为 `EventHub::MAX_EVENTS`；测试可以调小，
    /// 用少量事件造出确定性的断层。
    capacity: usize,
    /// 被环形缓冲挤出保留窗口的最大序号；`0` 表示还没丢过事件。
    evicted_through_sequence: u64,
    /// 按事件种别记录的挤出高水位。断层判据必须按种别给，否则只订阅
    /// `workspace.closed` 的客户端会因为 600 条 `pane.updated` 滚过窗口
    /// 就被要求重拉全量快照——而 `events.lost` 的语义正是「重新同步」，
    /// 代价按「连接 × 订阅」放大，恰好落在高负载工况里（HSR-03 的代价面）。
    evicted_through_by_kind: [u64; EventKind::COUNT],
    // 环形缓冲语义用 VecDeque 表达：溢出时从头弹出，不再整体左移。
    events: std::collections::VecDeque<(u64, EventEnvelope)>,
}

impl EventHubState {
    /// 某个种别（`None` = 任意种别）的挤出高水位：小于等于它的该种别序号都已
    /// 不可取。
    fn evicted_through(&self, kind: Option<EventKind>) -> u64 {
        match kind {
            Some(kind) => self.evicted_through_by_kind[kind.index()],
            None => self.evicted_through_sequence,
        }
    }

    fn push(&mut self, event: EventEnvelope) {
        self.next_sequence += 1;
        let sequence = self.next_sequence;
        self.events.push_back((sequence, event));
        while self.events.len() > self.capacity {
            let Some((evicted, evicted_event)) = self.events.pop_front() else {
                break;
            };
            self.evicted_through_sequence = evicted;
            self.evicted_through_by_kind[evicted_event.event.index()] = evicted;
        }
    }

    fn collect_after(&self, sequence: u64) -> Vec<(u64, EventEnvelope)> {
        self.events
            .iter()
            .filter(|(event_sequence, _)| *event_sequence > sequence)
            .cloned()
            .collect()
    }
}

/// `retained_after` 的结果：保留窗口内的事件，外加游标与窗口之间的断层。
/// 断层非空意味着调用方的游标之后有**它关心的那个种别**的事件已经被挤掉、
/// 不可恢复——静默丢弃正是 HSR-03/APP-006 的根因，所以这里把它做成返回值的
/// 一部分而不是日志。
///
/// 这个类型**刻意**不实现 `Deref`/`IntoIterator`：那样「把返回值当 Vec 用」的
/// 调用点会零改动编译通过，`gap` 被静默忽略而编译器不提醒。想要旧行为（不做
/// 断层判定）的调用方显式用 `EventHub::events_after`。
#[derive(Debug, Clone, Default)]
#[must_use = "断层必须显式处理：排队下发、触发探测，或明确记录后丢弃"]
pub struct RetainedEvents {
    pub events: Vec<(u64, EventEnvelope)>,
    pub gap: Option<EventGap>,
}

impl EventHub {
    const MAX_EVENTS: usize = 512;

    fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: std::sync::Arc::new(std::sync::Mutex::new(EventHubState {
                next_sequence: 0,
                capacity: capacity.max(1),
                evicted_through_sequence: 0,
                evicted_through_by_kind: [0; EventKind::COUNT],
                events: std::collections::VecDeque::new(),
            })),
        }
    }

    /// 测试专用：把环形缓冲容量调小，好用少量事件造出断层。
    #[cfg(test)]
    pub fn with_test_capacity(capacity: usize) -> Self {
        Self::with_capacity(capacity)
    }

    pub fn push(&self, event: EventEnvelope) {
        let Ok(mut state) = self.inner.lock() else {
            return;
        };
        state.push(event);
    }

    /// 测试专用：一次持锁推入一批事件。生产路径逐条 `push`；批量推入让
    /// 「订阅轮询恰好落在推入中间」的竞态消失，端到端断层测试才是确定性的。
    #[cfg(test)]
    pub fn push_batch(&self, events: Vec<EventEnvelope>) {
        let Ok(mut state) = self.inner.lock() else {
            return;
        };
        for event in events {
            state.push(event);
        }
    }

    /// 取游标之后仍保留着的事件，**不**做断层判定。生产路径一律走
    /// `retained_after`（返回值 `#[must_use]`，断层必须显式表态）；这个不带
    /// 断层的形式只留给断言事件已发出的测试，所以是 `#[cfg(test)]`——新的生产
    /// 调用点想「当 Vec 用」时会直接编译失败，而不是静默丢掉断层。
    #[cfg(test)]
    pub fn events_after(&self, sequence: u64) -> Vec<(u64, EventEnvelope)> {
        let Ok(state) = self.inner.lock() else {
            return Vec::new();
        };
        state.collect_after(sequence)
    }

    /// 取游标之后仍保留着的事件，并报告游标与保留窗口之间的断层。
    /// `kind` 给出调用方真正关心的事件种别：只有该种别真被挤掉才算断层；
    /// `None` 表示「任意种别被挤掉都算」（`agent.wait` 这类跨多种别的等待方
    /// 用它，多探测一次是廉价且自愈的，漏探测则会永久挂起）。
    pub fn retained_after(&self, sequence: u64, kind: Option<EventKind>) -> RetainedEvents {
        let Ok(state) = self.inner.lock() else {
            return RetainedEvents::default();
        };
        let evicted_through = state.evicted_through(kind);
        let gap = (evicted_through > sequence).then(|| EventGap {
            from: sequence.saturating_add(1),
            to: evicted_through,
        });
        RetainedEvents {
            events: state.collect_after(sequence),
            gap,
        }
    }

    pub fn current_sequence(&self) -> u64 {
        let Ok(state) = self.inner.lock() else {
            return 0;
        };
        state.next_sequence
    }

    /// 保留窗口内最老事件的序号（窗口为空时是下一条将分配的序号）。
    /// 生产路径读的是 `EventHubState::evicted_through`（`retained_after`
    /// 判定断层），这里只给测试断言窗口边界用。
    #[cfg(test)]
    pub fn oldest_retained_sequence(&self) -> u64 {
        let Ok(state) = self.inner.lock() else {
            return 1;
        };
        state.evicted_through_sequence + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{EventData, EventEnvelope, EventKind};

    fn workspace_focused_event(workspace_id: &str) -> EventEnvelope {
        EventEnvelope {
            event: EventKind::WorkspaceFocused,
            data: EventData::WorkspaceFocused {
                workspace_id: workspace_id.into(),
            },
        }
    }

    fn workspace_closed_event(workspace_id: &str) -> EventEnvelope {
        EventEnvelope {
            event: EventKind::WorkspaceClosed,
            data: EventData::WorkspaceClosed {
                workspace_id: workspace_id.into(),
                workspace: None,
            },
        }
    }

    #[test]
    fn event_hub_keeps_the_newest_events_and_drops_the_oldest() {
        let hub = EventHub::default();
        let pushed = EventHub::MAX_EVENTS + 88;
        for index in 0..pushed {
            hub.push(workspace_focused_event(&format!("workspace_{index}")));
        }

        assert_eq!(hub.current_sequence(), pushed as u64);

        let retained = hub.events_after(0);
        assert_eq!(retained.len(), EventHub::MAX_EVENTS);
        let first_retained_sequence = (pushed - EventHub::MAX_EVENTS + 1) as u64;
        assert_eq!(retained[0].0, first_retained_sequence);
        assert_eq!(retained[retained.len() - 1].0, pushed as u64);
    }

    /// HSR-03/APP-006 回归：环形缓冲挤掉事件后，落在窗口之外的游标必须拿到
    /// 断层区间，而不是静默少几条。
    #[test]
    fn retained_after_reports_the_gap_left_by_the_ring_buffer() {
        let hub = EventHub::default();
        let pushed = EventHub::MAX_EVENTS + 88;
        for index in 0..pushed {
            hub.push(workspace_focused_event(&format!("workspace_{index}")));
        }

        let retained = hub.retained_after(0, None);
        assert_eq!(
            retained.gap,
            Some(EventGap { from: 1, to: 88 }),
            "游标 0 之后的 1..=88 号事件已被挤掉"
        );
        assert_eq!(retained.events.len(), EventHub::MAX_EVENTS);
        assert_eq!(hub.oldest_retained_sequence(), 89);

        // 游标正好落在断层边界上：89 号仍在窗口里，89 之后无断层。
        assert!(hub.retained_after(88, None).gap.is_none());
        // 游标落在断层中间：只报还没送达的那一段。
        assert_eq!(
            hub.retained_after(40, None).gap,
            Some(EventGap { from: 41, to: 88 })
        );
    }

    #[test]
    fn retained_after_reports_no_gap_while_everything_is_still_retained() {
        let hub = EventHub::default();
        assert_eq!(hub.oldest_retained_sequence(), 1);
        assert!(hub.retained_after(0, None).gap.is_none());

        for index in 0..EventHub::MAX_EVENTS {
            hub.push(workspace_focused_event(&format!("workspace_{index}")));
        }

        assert_eq!(hub.oldest_retained_sequence(), 1);
        assert!(hub.retained_after(0, None).gap.is_none());
        assert!(hub
            .retained_after(hub.current_sequence(), None)
            .gap
            .is_none());
    }

    /// 断层判据按种别给：挤掉的全是订阅方不关心的种别时不得报断层，否则每次
    /// 事件突发都会让所有低频订阅方做一次无谓的全量快照重拉。
    #[test]
    fn retained_after_ignores_evictions_of_other_event_kinds() {
        let hub = EventHub::with_test_capacity(4);
        // 先推一条 workspace.closed，让它稳稳留在窗口里。
        hub.push(workspace_closed_event("kept"));
        // 再用 workspace.focused 把窗口挤到只剩最后 4 条——被挤掉的只有
        // focused，closed 那条也被挤了吗？没有：窗口只有 4 格，先确认边界。
        for index in 0..3 {
            hub.push(workspace_focused_event(&format!("noise_{index}")));
        }
        assert!(
            hub.retained_after(0, Some(EventKind::WorkspaceClosed))
                .gap
                .is_none(),
            "还没挤掉任何事件"
        );

        // 再推 6 条 focused：1..=6 号被挤掉，其中 1 号是 closed。
        for index in 3..9 {
            hub.push(workspace_focused_event(&format!("noise_{index}")));
        }
        assert_eq!(
            hub.retained_after(0, Some(EventKind::WorkspaceFocused)).gap,
            Some(EventGap { from: 1, to: 6 }),
            "focused 确实被挤掉了"
        );
        assert_eq!(
            hub.retained_after(0, Some(EventKind::WorkspaceClosed)).gap,
            Some(EventGap { from: 1, to: 1 }),
            "1 号 closed 也被挤掉，区间收敛到它自己而不是整个全局区间"
        );

        // 游标越过那条 closed 之后，再多的 focused 噪声也不该报断层。
        assert!(
            hub.retained_after(1, Some(EventKind::WorkspaceClosed))
                .gap
                .is_none(),
            "被挤掉的全是订阅方不关心的种别时不得报断层"
        );
        // 但全局判据（`None`）仍然看得见：`agent.wait` 靠它兜底探测。
        assert_eq!(
            hub.retained_after(1, None).gap,
            Some(EventGap { from: 2, to: 6 })
        );
    }

    #[test]
    fn events_after_only_returns_events_newer_than_the_cursor() {
        let hub = EventHub::default();
        hub.push(workspace_focused_event("first"));
        let cursor = hub.current_sequence();
        hub.push(workspace_focused_event("second"));
        hub.push(workspace_focused_event("third"));

        let events = hub.events_after(cursor);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].0, cursor + 1);
        assert_eq!(events[1].0, cursor + 2);
        assert!(hub.events_after(hub.current_sequence()).is_empty());
    }

    /// `EventKind::index` 是定长数组的下标：必须两两不同且恰好铺满 `COUNT`，
    /// 否则按种别记录的挤出高水位会串种别（或越界 panic）。
    #[test]
    fn event_kind_indices_cover_every_slot_exactly_once() {
        let mut seen = [false; EventKind::COUNT];
        for kind in crate::api::schema::KNOWN_EVENT_KINDS {
            let index = kind.index();
            assert!(index < EventKind::COUNT, "{kind:?} 的下标越界");
            assert!(!seen[index], "{kind:?} 的下标与其它种别重复");
            seen[index] = true;
        }
        assert!(seen.iter().all(|slot| *slot), "有下标没有被任何种别占用");
    }
}
