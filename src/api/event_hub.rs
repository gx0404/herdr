#[derive(Clone, Default)]
pub struct EventHub {
    inner: std::sync::Arc<std::sync::Mutex<EventHubState>>,
}

#[derive(Default)]
struct EventHubState {
    next_sequence: u64,
    // 环形缓冲语义用 VecDeque 表达：溢出时从头弹出，不再整体左移。
    events: std::collections::VecDeque<(u64, crate::api::schema::EventEnvelope)>,
}

impl EventHub {
    const MAX_EVENTS: usize = 512;

    pub fn push(&self, event: crate::api::schema::EventEnvelope) {
        let Ok(mut state) = self.inner.lock() else {
            return;
        };
        state.next_sequence += 1;
        let sequence = state.next_sequence;
        state.events.push_back((sequence, event));
        while state.events.len() > Self::MAX_EVENTS {
            state.events.pop_front();
        }
    }

    pub fn events_after(&self, sequence: u64) -> Vec<(u64, crate::api::schema::EventEnvelope)> {
        let Ok(state) = self.inner.lock() else {
            return Vec::new();
        };
        state
            .events
            .iter()
            .filter(|(event_sequence, _)| *event_sequence > sequence)
            .cloned()
            .collect()
    }

    pub fn current_sequence(&self) -> u64 {
        let Ok(state) = self.inner.lock() else {
            return 0;
        };
        state.next_sequence
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
}
