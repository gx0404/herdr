use super::*;

type EndpointAttentionKey = (ClientEndpointId, Option<(u64, String)>, bool);

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentAttentionKey {
    data_epoch: u64,
    activity_epoch: u64,
    endpoints: Vec<EndpointAttentionKey>,
}

pub(super) struct AgentAttentionCache {
    key: AgentAttentionKey,
    counts: crate::harness::BucketCounts,
    animated: bool,
}

/// 在线端点快照里每个 pane agent 的状态桶计数，以及有没有需要转圈的状态。离线端点
/// 的缓存快照照常画成 stale 行，但不计数、也不让转圈时钟走。
fn agent_attention(endpoints: &[ClientShellEndpoint]) -> (crate::harness::BucketCounts, bool) {
    use crate::api::schema::{AgentActivityStatus, AgentStatus};
    let running = |activity: &crate::protocol::ClientShellAgentActivity| {
        activity
            .nodes
            .iter()
            .any(|node| node.status == AgentActivityStatus::Running)
    };
    let mut counts = crate::harness::BucketCounts::default();
    let mut animated = false;
    for snapshot in endpoints
        .iter()
        .filter(|endpoint| endpoint.status == ClientEndpointStatus::Online)
        .filter_map(|endpoint| endpoint.snapshot.as_deref())
    {
        for agent in &snapshot.agents {
            counts.add(super::client_agent_bucket(agent));
            animated |= agent.agent_status == AgentStatus::Working || running(&agent.activity);
        }
        animated |= snapshot.external_agents.iter().any(|external| {
            external.agent_status == AgentStatus::Working || running(&external.activity)
        });
    }
    (counts, animated)
}

impl AgentAttentionCache {
    pub(super) fn counts(&self) -> crate::harness::BucketCounts {
        self.counts
    }

    pub(super) fn animated(&self) -> bool {
        self.animated
    }
}

impl ClientShellState {
    pub(super) fn reveal_endpoint_agent(
        &mut self,
        endpoint_id: &ClientEndpointId,
        pane_id: &str,
        _body_height: u16,
    ) {
        self.reveal_session_target = Some((endpoint_id.clone(), pane_id.to_owned()));
    }

    pub(super) fn reveal_collapsed_endpoint_agent(
        &mut self,
        endpoint_id: &ClientEndpointId,
        pane_id: &str,
    ) {
        self.reveal_session_target = Some((endpoint_id.clone(), pane_id.to_owned()));
    }

    pub(super) fn refresh_agent_attention(&mut self) {
        let key = AgentAttentionKey {
            data_epoch: self.agent_rows_epoch,
            activity_epoch: self.agent_activity_epoch,
            endpoints: self
                .endpoints
                .iter()
                .map(|endpoint| {
                    (
                        endpoint.endpoint_id.clone(),
                        endpoint
                            .snapshot
                            .as_deref()
                            .map(|snapshot| (snapshot.revision, snapshot.boot_id.clone())),
                        endpoint.status == ClientEndpointStatus::Online,
                    )
                })
                .collect(),
        };
        if self
            .agent_attention
            .as_ref()
            .is_none_or(|cache| cache.key != key)
        {
            let (counts, animated) = agent_attention(&self.endpoints);
            self.agent_attention = Some(AgentAttentionCache {
                key,
                counts,
                animated,
            });
        }
    }
}
