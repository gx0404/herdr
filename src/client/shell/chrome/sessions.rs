use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use serde::{Deserialize, Serialize};

use super::super::agent_activity_overlay::AgentActivityOwner;
use super::super::agent_sidebar::agent_group_key;
use super::super::agent_tree::{
    external_group_key, external_sources, render_session_activity_row, session_activity,
    AgentTreeRow, CollapseState,
};
use super::super::aggregate_navigation::{cached_endpoint_snapshots, CachedEndpointSnapshot};
use super::super::dock::{Geometry, PanelId};
use super::super::feedback::ChromeHover;
use super::super::observability::tr;
use super::super::*;
use super::{paint_spans, Span};
use crate::harness::{AgentBucket, BucketCounts};
use crate::protocol::{ClientShellAgent, ClientShellSnapshot, ClientShellWorkspace};
use crate::ui::{display_width_u16, truncate_end};

pub(in crate::client::shell) const RAIL_BELOW_COLS: u16 = 100;
pub(in crate::client::shell) const RAIL_WIDTH: u16 = 4;
const RAIL_BELOW_PANEL: u16 = 16;
const INSET: u16 = 1;
const SUMMARY_MIN: u16 = 4;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::client::shell) enum SessionGrouping {
    #[default]
    Workspace,
    Status,
}

impl SessionGrouping {
    pub(in crate::client::shell) fn toggled(self) -> Self {
        match self {
            Self::Workspace => Self::Status,
            Self::Status => Self::Workspace,
        }
    }

    fn toggle_label(self) -> &'static str {
        match self {
            Self::Workspace => tr("By workspace ▾", "按工作区 ▾"),
            Self::Status => tr("By status ▾", "按状态 ▾"),
        }
    }

    fn section_label(self) -> &'static str {
        match self {
            Self::Workspace => tr("Workspaces", "工作区"),
            Self::Status => tr("Status", "状态"),
        }
    }
}

pub(in crate::client::shell) fn bucket_label(bucket: AgentBucket) -> &'static str {
    match bucket {
        AgentBucket::NeedsInput => tr("Needs input", "需要输入"),
        AgentBucket::Failed => tr("Failed", "失败"),
        AgentBucket::Review => tr("Review", "待查看"),
        AgentBucket::Working => tr("Working", "运行中"),
        AgentBucket::Idle => tr("Idle", "空闲"),
        AgentBucket::None => tr("Other", "其它"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::client::shell) enum SessionHit {
    NewWorkspace,
    Search,
    Overview,
    Grouping,
    Toggle(ClientEndpointId, String),
    Terminal(ClientEndpointId, String),
    Tab(ClientEndpointId, String),
    Settings,
    Help,
    Machines,
    Rail(ClientEndpointId, String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::client::shell) struct Rollup {
    pub(in crate::client::shell) bucket: AgentBucket,
    pub(in crate::client::shell) count: usize,
}

impl Rollup {
    fn of(buckets: impl Iterator<Item = AgentBucket>) -> Option<Self> {
        let mut top = AgentBucket::None;
        let mut count = 0usize;
        for bucket in buckets {
            match bucket.rollup_rank().cmp(&top.rollup_rank()) {
                std::cmp::Ordering::Greater => {
                    top = bucket;
                    count = 1;
                }
                std::cmp::Ordering::Equal => count += 1,
                std::cmp::Ordering::Less => {}
            }
        }
        (!matches!(top, AgentBucket::Idle | AgentBucket::None))
            .then_some(Self { bucket: top, count })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::client::shell) struct SessionAgent {
    pub(in crate::client::shell) pane_id: String,
    pub(in crate::client::shell) bucket: AgentBucket,
    pub(in crate::client::shell) name: String,
    pub(in crate::client::shell) location: Option<String>,
    pub(in crate::client::shell) summary: String,
    pub(in crate::client::shell) tokens: Vec<Vec<crate::ui::ResolvedToken>>,
    pub(in crate::client::shell) age: String,
    pub(in crate::client::shell) focused: bool,
    pub(in crate::client::shell) indent: u16,
    pub(in crate::client::shell) activity: Option<(String, bool, String)>,
    pub(in crate::client::shell) activity_running: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::client::shell) enum MachineTag {
    Color(ratatui::style::Color),
    Hashed,
}

#[derive(Debug)]
pub(in crate::client::shell) enum SessionRowKind {
    MachineGroup {
        label: String,
    },
    Machine {
        label: String,
        status: ClientEndpointStatus,
        collapsed: bool,
        rollup: Option<Rollup>,
        tag: Option<MachineTag>,
    },
    Workspace {
        workspace_id: String,
        label: String,
        meta: Option<String>,
        mobile_meta: String,
        extras: Vec<crate::ui::ResolvedToken>,
        focused: bool,
        indent: u16,
        indented: bool,
        last_child: bool,
        toggle: Option<(String, bool)>,
        rollup: Option<Rollup>,
        holds_focus: bool,
    },
    Tab {
        tab_id: String,
        label: String,
        key: String,
        collapsed: bool,
        rollup: Option<Rollup>,
        indent: u16,
        holds_focus: bool,
    },
    Agent(SessionAgent),
    Terminal {
        pane_id: String,
        title: String,
        focused: bool,
        indent: u16,
    },
    Activity(AgentTreeRow, u16),
    ExternalGroup {
        key: String,
        label: String,
        collapsed: bool,
        count: String,
    },
    External {
        external_id: String,
        label: String,
        agent_name: Option<String>,
        readable: bool,
        bucket: AgentBucket,
        age: String,
        activity: Option<(String, bool, String)>,
    },
    Section {
        bucket: AgentBucket,
        label: String,
    },
    MobileAction {
        label: String,
        target: Option<ClientMobileTarget>,
        focused: bool,
    },
    Empty(&'static str),
}

#[derive(Debug)]
pub(in crate::client::shell) struct SessionRow {
    pub(in crate::client::shell) endpoint_id: ClientEndpointId,
    pub(in crate::client::shell) stale: bool,
    pub(in crate::client::shell) kind: SessionRowKind,
}

impl SessionRow {
    fn height(&self) -> u16 {
        match &self.kind {
            SessionRowKind::Workspace { meta: Some(_), .. } => 2,
            SessionRowKind::Agent(agent) => agent.tokens.len().max(1).min(u16::MAX as usize) as u16,
            _ => 1,
        }
    }

    fn height_for(&self, mobile: bool) -> u16 {
        if mobile && matches!(self.kind, SessionRowKind::Workspace { .. }) {
            2
        } else {
            self.height()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::client::shell) struct RailItem {
    pub(in crate::client::shell) endpoint_id: ClientEndpointId,
    pub(in crate::client::shell) workspace_id: String,
    pub(in crate::client::shell) number: String,
    pub(in crate::client::shell) bucket: AgentBucket,
    pub(in crate::client::shell) focused: bool,
    pub(in crate::client::shell) stale: bool,
    machine: Option<(String, ClientEndpointStatus, Option<MachineTag>, bool)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::client::shell) struct SessionRowsKey {
    config_epoch: u64,
    data_epoch: u64,
    collapse_epoch: u64,
    activity_epoch: u64,
    active_endpoint: ClientEndpointId,
    grouping: SessionGrouping,
    sort: crate::config::AgentPanelSortConfig,
    minute: u64,
    endpoints: Vec<EndpointKey>,
}

type EndpointKey = (
    ClientEndpointId,
    Option<(u64, String)>,
    String,
    ClientEndpointStatus,
);

pub(in crate::client::shell) struct SessionRowsCache {
    key: SessionRowsKey,
    pub(in crate::client::shell) rows: Vec<SessionRow>,
    pub(in crate::client::shell) heights: Vec<u16>,
    pub(in crate::client::shell) rail: Vec<RailItem>,
    pub(in crate::client::shell) counts: BucketCounts,
    shows_ages: bool,
    mobile_tail: Vec<SessionRow>,
    mobile_heights: Vec<u16>,
    pub(in crate::client::shell) gaps: Vec<u16>,
    mobile_gaps: Vec<u16>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::client::shell) struct SessionView {
    pub(in crate::client::shell) area: Rect,
    pub(in crate::client::shell) rail: bool,
    pub(in crate::client::shell) mobile: bool,
    pub(in crate::client::shell) header: Rect,
    pub(in crate::client::shell) agents: Rect,
    pub(in crate::client::shell) sidebar_divider: Rect,
    pub(in crate::client::shell) section_divider: Rect,
    pub(in crate::client::shell) sidebar_toggle: Rect,
    pub(in crate::client::shell) overview: Rect,
    pub(in crate::client::shell) grouping: Rect,
    pub(in crate::client::shell) body: Rect,
    pub(in crate::client::shell) footer: Rect,
    pub(in crate::client::shell) start: usize,
    pub(in crate::client::shell) metrics: Option<crate::pane::ScrollMetrics>,
    pub(in crate::client::shell) selected: Option<WorkspaceNavigationTarget>,
    pub(in crate::client::shell) dragged: Option<String>,
    pub(in crate::client::shell) drop_row: Option<u16>,
}

pub(in crate::client::shell) fn session_summary(
    agent: &ClientShellAgent,
    bucket: AgentBucket,
) -> String {
    let insight = &agent.insight;
    let attention = insight.attention.as_ref();
    let attention_detail = attention.and_then(|attention| attention.detail.as_deref());
    let attention_title = attention.and_then(|attention| attention.title.as_deref());
    let candidates: [Option<&str>; 3] = match bucket {
        AgentBucket::NeedsInput => [
            attention_detail,
            insight.question.as_deref(),
            attention_title,
        ],
        AgentBucket::Failed => [attention_detail, insight.error.as_deref(), attention_title],
        AgentBucket::Working => [
            insight.current_tool.as_deref(),
            insight.session_title.as_deref(),
            None,
        ],
        _ => [
            insight.session_title.as_deref(),
            insight.last_message.as_deref(),
            None,
        ],
    };
    let name = agent_name(agent);
    let fallback = agent
        .terminal_title_stripped
        .as_deref()
        .or(agent.title.as_deref())
        .filter(|title| !title.trim().eq_ignore_ascii_case(name.trim()));
    candidates
        .into_iter()
        .flatten()
        .chain(fallback)
        .map(single_line)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
}

fn single_line(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let mut out = String::with_capacity(line.len());
    for word in line.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

pub(in crate::client::shell) fn session_age(
    agent: &ClientShellAgent,
    bucket: AgentBucket,
    now_ms: u64,
) -> String {
    let insight = &agent.insight;
    let since = if bucket == AgentBucket::Working {
        insight.working_since_ms.or(insight.state_changed_at_ms)
    } else {
        insight.state_changed_at_ms
    };
    since.map_or_else(String::new, |since| {
        minute_age(now_ms.saturating_sub(since))
    })
}

fn minute_age(elapsed_ms: u64) -> String {
    if elapsed_ms < 60_000 {
        "now".to_owned()
    } else {
        crate::ui::design::compact_age(elapsed_ms)
    }
}

fn agent_name(agent: &ClientShellAgent) -> String {
    agent
        .display_agent
        .as_deref()
        .or(agent.name.as_deref())
        .or(agent.agent.as_deref())
        .or(agent.title.as_deref())
        .unwrap_or("agent")
        .to_owned()
}

fn terminal_title(pane: &crate::protocol::ClientShellPane) -> String {
    if let Some(label) = pane.label.as_deref().filter(|label| !label.is_empty()) {
        return label.to_owned();
    }
    pane.foreground_cwd
        .as_deref()
        .or(pane.cwd.as_deref())
        .and_then(|cwd| {
            cwd.trim_end_matches(['/', '\\'])
                .rsplit(['/', '\\'])
                .next()
                .filter(|name| !name.is_empty())
        })
        .map_or_else(|| tr("terminal", "终端").to_owned(), str::to_owned)
}

fn workspace_meta(
    workspace: &ClientShellWorkspace,
    indented: bool,
    config: &crate::config::SpacesSidebarConfig,
) -> Option<String> {
    let mut meta = if indented {
        String::new()
    } else {
        workspace.branch.clone().unwrap_or_default()
    };
    if !indented {
        if let Some((ahead, behind)) = workspace.git_ahead_behind {
            if ahead > 0 {
                meta.push_str(&format!(" ↑{ahead}"));
            }
            if behind > 0 {
                meta.push_str(&format!(" ↓{behind}"));
            }
        }
    }
    let tokens = workspace.tokens.iter().cloned().collect::<HashMap<_, _>>();
    let rows = crate::ui::sidebar_space_rows(
        config,
        crate::ui::SpaceTokenContext {
            workspace: &workspace.label,
            branch: workspace.branch.as_deref(),
            state_text: status_text(workspace.agent_status),
            ahead_behind: workspace.git_ahead_behind,
            tokens: &tokens,
            suppress_git_details: indented,
        },
    );
    for token in rows.into_iter().flatten() {
        if let crate::ui::ResolvedTokenKind::Custom(value) = token.kind {
            if !meta.is_empty() {
                meta.push_str(" · ");
            }
            meta.push_str(&value);
        }
    }
    (!meta.is_empty()).then_some(meta)
}

fn workspace_extra_tokens(
    workspace: &ClientShellWorkspace,
    indented: bool,
    config: &crate::config::SpacesSidebarConfig,
) -> Vec<crate::ui::ResolvedToken> {
    let tokens = workspace.tokens.iter().cloned().collect::<HashMap<_, _>>();
    crate::ui::sidebar_space_rows(
        config,
        crate::ui::SpaceTokenContext {
            workspace: &workspace.label,
            branch: workspace.branch.as_deref(),
            state_text: status_text(workspace.agent_status),
            ahead_behind: workspace.git_ahead_behind,
            tokens: &tokens,
            suppress_git_details: indented,
        },
    )
    .into_iter()
    .flatten()
    .filter(|token| matches!(token.kind, crate::ui::ResolvedTokenKind::StateText(_)))
    .collect()
}

fn workspace_label(workspace: &ClientShellWorkspace, indented: bool) -> String {
    if indented && !workspace.custom_label {
        if let Some(branch) = workspace.branch.as_deref() {
            return branch
                .strip_prefix("worktree/")
                .unwrap_or(branch)
                .to_owned();
        }
    }
    workspace.label.clone()
}

fn workspace_mobile_meta(
    snapshot: &ClientShellSnapshot,
    workspace: &ClientShellWorkspace,
    indented: bool,
    config: &crate::config::SpacesSidebarConfig,
) -> String {
    let tabs = snapshot
        .tabs
        .iter()
        .filter(|tab| tab.workspace_id == workspace.workspace_id)
        .collect::<Vec<_>>();
    let active = tabs
        .iter()
        .position(|tab| tab.tab_id == workspace.active_tab_id)
        .unwrap_or(0);
    let tab = tabs.get(active).map_or_else(String::new, |tab| {
        if tabs.len() <= 1 {
            crate::i18n::fill(
                crate::i18n::texts().mobile.tab_label_fmt,
                &[("label", &tab.label)],
            )
        } else {
            crate::i18n::fill(
                crate::i18n::texts().mobile.tab_label_pos_fmt,
                &[
                    ("label", &tab.label),
                    ("active", &(active + 1).to_string()),
                    ("total", &tabs.len().to_string()),
                ],
            )
        }
    });
    let meta = workspace_meta(workspace, indented, config).unwrap_or_else(|| "shell".into());
    if tab.is_empty() {
        meta
    } else {
        format!("{meta} · {tab}")
    }
}

fn worktree_parent_key(
    snapshot: &ClientShellSnapshot,
    workspace: &ClientShellWorkspace,
) -> Option<String> {
    let worktree = workspace.worktree.as_ref()?;
    if worktree.is_linked_worktree {
        return None;
    }
    snapshot
        .workspaces
        .iter()
        .any(|candidate| {
            candidate.worktree.as_ref().is_some_and(|candidate| {
                candidate.key == worktree.key && candidate.is_linked_worktree
            })
        })
        .then(|| worktree.key.clone())
}

fn workspace_panes<'a>(
    snapshot: &'a ClientShellSnapshot,
    workspace_id: &'a str,
) -> impl Iterator<Item = &'a crate::protocol::ClientShellPane> + 'a {
    snapshot
        .tabs
        .iter()
        .filter(move |tab| tab.workspace_id == workspace_id)
        .flat_map(move |tab| {
            snapshot
                .panes
                .iter()
                .filter(move |pane| pane.tab_id == tab.tab_id)
        })
}

fn pane_agent<'a>(
    snapshot: &'a ClientShellSnapshot,
    pane_id: &str,
) -> Option<&'a ClientShellAgent> {
    snapshot
        .agents
        .iter()
        .find(|agent| agent.pane_id == pane_id)
}

struct BuildInput<'a> {
    endpoints: &'a [ClientShellEndpoint],
    active_endpoint_id: &'a ClientEndpointId,
    config: &'a ClientShellConfig,
    collapse: &'a CollapseState<'a>,
    machine_chrome: &'a HashMap<crate::client::endpoint::ProfileId, MachineChrome>,
    grouping: SessionGrouping,
    now_ms: u64,
    ordered_agents: Vec<super::super::aggregate_navigation::AggregateAgentRow<'a>>,
}

impl BuildInput<'_> {
    fn panes<'s>(
        &self,
        endpoint: CachedEndpointSnapshot<'s>,
        workspace: &'s str,
    ) -> Vec<&'s crate::protocol::ClientShellPane> {
        let rank = |pane: &crate::protocol::ClientShellPane| {
            self.ordered_agents.iter().position(|row| {
                row.endpoint.endpoint_id == endpoint.endpoint_id
                    && row.agent.pane_id == pane.pane_id
            })
        };
        let mut panes = workspace_panes(endpoint.snapshot, workspace)
            .filter(|pane| {
                pane_agent(endpoint.snapshot, &pane.pane_id).is_none() || rank(pane).is_some()
            })
            .collect::<Vec<_>>();
        if self.config.agent_panel_sort == crate::config::AgentPanelSortConfig::Launch
            || endpoint.snapshot.agent_view_label.is_some()
        {
            panes.sort_by_key(|pane| rank(pane).map_or((false, 0), |rank| (true, rank)));
        }
        panes
    }

    fn machine_group(&self, endpoint_id: &ClientEndpointId) -> Option<&str> {
        let ClientEndpointId::Ssh(profile_id) = endpoint_id else {
            return None;
        };
        self.machine_chrome
            .get(profile_id)
            .and_then(|chrome| chrome.group.as_deref())
    }

    fn machine_tag(&self, endpoint_id: &ClientEndpointId) -> Option<MachineTag> {
        let ClientEndpointId::Ssh(profile_id) = endpoint_id else {
            return None;
        };
        Some(
            self.machine_chrome
                .get(profile_id)
                .and_then(|chrome| chrome.color)
                .map_or(MachineTag::Hashed, MachineTag::Color),
        )
    }

    fn groups(&self, endpoint_id: &ClientEndpointId) -> Option<&HashSet<String>> {
        if endpoint_id.is_local() {
            Some(self.collapse.collapsed_groups)
        } else {
            self.collapse.remote_collapsed_groups.get(endpoint_id)
        }
    }

    fn collapsed(&self, endpoint_id: &ClientEndpointId, key: &str) -> bool {
        self.groups(endpoint_id)
            .is_some_and(|groups| groups.contains(key))
    }

    fn focused_pane<'s>(&self, endpoint: CachedEndpointSnapshot<'s>) -> Option<&'s str> {
        (endpoint.endpoint_id == self.active_endpoint_id)
            .then_some(endpoint.snapshot.focused_pane_id.as_deref())
            .flatten()
    }

    fn agent_row(
        &self,
        endpoint: CachedEndpointSnapshot<'_>,
        agent: &ClientShellAgent,
        indent: u16,
        location: Option<String>,
        with_activity: bool,
    ) -> (SessionAgent, Vec<AgentTreeRow>) {
        let bucket = client_agent_bucket(agent);
        let (chip, children) = self.activity_rows(
            endpoint,
            &AgentActivityOwner::Pane {
                pane_id: agent.pane_id.clone(),
            },
            agent.agent_status,
            &agent.activity,
            with_activity,
        );
        let canonical = agent
            .agent
            .as_deref()
            .and_then(crate::detect::parse_agent_label);
        let configured = self.config.agents.rows_for_agent(canonical);
        use crate::config::AgentSidebarToken as Token;
        let native = matches!(configured.as_slice(), [first, second]
            if first.as_slice() == [Token::StateIcon, Token::Machine, Token::Workspace, Token::Tab]
                && second.as_slice() == [Token::Agent]);
        let tokens = if native {
            Vec::new()
        } else {
            let mut rows = super::super::agent_sidebar::agent_row(
                endpoint.snapshot,
                &agent.pane_id,
                self.config,
                location.as_ref().map(|_| endpoint.label),
            )
            .unwrap_or_default();
            if location.is_none() {
                let drop_tab = endpoint
                    .snapshot
                    .tabs
                    .iter()
                    .filter(|tab| tab.workspace_id == agent.workspace_id)
                    .count()
                    > 1;
                for row in &mut rows {
                    row.retain(|token| {
                        !(matches!(token.kind, crate::ui::ResolvedTokenKind::Workspace(_))
                            || drop_tab
                                && matches!(token.kind, crate::ui::ResolvedTokenKind::Tab(_)))
                    });
                }
                rows.retain(|row| !row.is_empty());
                if rows.len() > 1
                    && rows[0]
                        .iter()
                        .all(|token| matches!(token.kind, crate::ui::ResolvedTokenKind::StateIcon))
                {
                    let icons = rows.remove(0);
                    rows[0].splice(0..0, icons);
                }
            }
            rows
        };
        let summary = agent
            .state_labels
            .iter()
            .find(|(state, _)| state == status_text(agent.agent_status))
            .map_or_else(
                || session_summary(agent, bucket),
                |(_, label)| label.clone(),
            );
        (
            SessionAgent {
                pane_id: agent.pane_id.clone(),
                bucket,
                name: agent_name(agent),
                location,
                summary,
                tokens,
                age: session_age(agent, bucket, self.now_ms),
                focused: self.focused_pane(endpoint) == Some(agent.pane_id.as_str()),
                indent,
                activity: chip,
                activity_running: agent.activity.running > 0,
            },
            children,
        )
    }

    fn activity_rows(
        &self,
        endpoint: CachedEndpointSnapshot<'_>,
        owner: &AgentActivityOwner,
        status: crate::api::schema::AgentStatus,
        activity: &crate::protocol::ClientShellAgentActivity,
        enabled: bool,
    ) -> (Option<(String, bool, String)>, Vec<AgentTreeRow>) {
        if !enabled {
            return (None, Vec::new());
        }
        let activity = session_activity(
            endpoint,
            owner,
            status,
            activity,
            self.config,
            self.collapse,
        );
        if !activity.has_children && activity.count == 0 {
            return (None, Vec::new());
        }
        let marker = if activity.expanded { "▾" } else { "▸" };
        (
            Some((
                activity.key,
                activity.expanded,
                format!("{marker}{}", activity.count),
            )),
            activity.rows,
        )
    }

    fn external_row(
        &self,
        endpoint: CachedEndpointSnapshot<'_>,
        external: &crate::protocol::ClientShellExternalAgent,
    ) -> (SessionRowKind, Vec<AgentTreeRow>) {
        let (activity, children) = self.activity_rows(
            endpoint,
            &AgentActivityOwner::External {
                external_id: external.external_id.clone(),
            },
            external.agent_status,
            &external.activity,
            true,
        );
        (
            SessionRowKind::External {
                external_id: external.external_id.clone(),
                label: if external.label.is_empty() {
                    external.external_id.clone()
                } else {
                    external.label.clone()
                },
                agent_name: external.agent.clone().filter(|name| !name.is_empty()),
                readable: external.readable,
                bucket: agent_bucket(external.agent_status),
                age: external.updated_at_ms.map_or_else(String::new, |since| {
                    minute_age(self.now_ms.saturating_sub(since))
                }),
                activity,
            },
            children,
        )
    }
}

fn build_rows(input: &BuildInput<'_>) -> (Vec<SessionRow>, Vec<RailItem>, BucketCounts) {
    let mut rows = Vec::new();
    let mut rail = Vec::new();
    let mut counts = BucketCounts::default();
    for endpoint in cached_endpoint_snapshots(input.endpoints) {
        if !endpoint.stale() {
            for agent in &endpoint.snapshot.agents {
                counts.add(client_agent_bucket(agent));
            }
        }
    }
    for endpoint in input.endpoints {
        let active = &endpoint.endpoint_id == input.active_endpoint_id;
        let stale = endpoint.status != ClientEndpointStatus::Online;
        let collapsed = input
            .collapse
            .collapsed_endpoints
            .contains(&endpoint.endpoint_id);
        if input.endpoints.len() > 1 {
            rail.push(RailItem {
                endpoint_id: endpoint.endpoint_id.clone(),
                workspace_id: String::new(),
                number: endpoint
                    .label
                    .chars()
                    .next()
                    .map(|letter| letter.to_uppercase().collect())
                    .unwrap_or_default(),
                bucket: AgentBucket::None,
                focused: active && collapsed,
                stale,
                machine: Some((
                    endpoint.label.clone(),
                    endpoint.status,
                    input.machine_tag(&endpoint.endpoint_id),
                    collapsed,
                )),
            });
        }
        if collapsed && input.endpoints.len() > 1 {
            continue;
        }
        let Some(snapshot) = endpoint.snapshot.as_deref() else {
            continue;
        };
        for workspace in &snapshot.workspaces {
            rail.push(RailItem {
                endpoint_id: endpoint.endpoint_id.clone(),
                workspace_id: workspace.workspace_id.clone(),
                number: workspace.number.to_string(),
                bucket: {
                    let mut agents = snapshot
                        .agents
                        .iter()
                        .filter(|agent| agent.workspace_id == workspace.workspace_id)
                        .peekable();
                    if agents.peek().is_none() {
                        agent_bucket(workspace.agent_status)
                    } else {
                        AgentBucket::rollup(agents.map(client_agent_bucket))
                    }
                },
                focused: active && workspace.focused,
                stale,
                machine: None,
            });
        }
    }
    match input.grouping {
        SessionGrouping::Workspace => {
            build_by_workspace(input, &mut rows);
            let custom = input
                .endpoints
                .iter()
                .find(|endpoint| &endpoint.endpoint_id == input.active_endpoint_id)
                .and_then(|endpoint| endpoint.snapshot.as_deref())
                .is_some_and(|snapshot| snapshot.agent_view_label.is_some());
            if custom
                || input.config.agent_panel_sort == crate::config::AgentPanelSortConfig::Launch
            {
                rows.retain(|row| {
                    matches!(
                        row.kind,
                        SessionRowKind::MachineGroup { .. }
                            | SessionRowKind::Machine { .. }
                            | SessionRowKind::Workspace { .. }
                            | SessionRowKind::Terminal { .. }
                    )
                });
                for ordered in &input.ordered_agents {
                    let workspace = ordered
                        .endpoint
                        .snapshot
                        .workspaces
                        .iter()
                        .find(|workspace| workspace.workspace_id == ordered.agent.workspace_id);
                    let location = workspace.map(|workspace| {
                        if input.endpoints.len() > 1 {
                            format!("{}/{}", ordered.endpoint.label, workspace.label)
                        } else {
                            workspace.label.clone()
                        }
                    });
                    let (agent, children) =
                        input.agent_row(ordered.endpoint, ordered.agent, 0, location, true);
                    rows.push(SessionRow {
                        endpoint_id: ordered.endpoint.endpoint_id.clone(),
                        stale: ordered.endpoint.stale(),
                        kind: SessionRowKind::Agent(agent),
                    });
                    rows.extend(children.into_iter().map(|child| SessionRow {
                        endpoint_id: ordered.endpoint.endpoint_id.clone(),
                        stale: ordered.endpoint.stale(),
                        kind: SessionRowKind::Activity(child, 0),
                    }));
                }
                if !custom {
                    for endpoint in cached_endpoint_snapshots(input.endpoints) {
                        for source in external_sources(&endpoint.snapshot.external_agents) {
                            let key = external_group_key(source);
                            let collapsed = input.collapsed(endpoint.endpoint_id, &key);
                            let members = endpoint
                                .snapshot
                                .external_agents
                                .iter()
                                .filter(|external| external.source == source);
                            rows.push(SessionRow {
                                endpoint_id: endpoint.endpoint_id.clone(),
                                stale: endpoint.stale(),
                                kind: SessionRowKind::ExternalGroup {
                                    key,
                                    label: format!(
                                        "{} · {source}",
                                        crate::i18n::texts().agent_panel.external_group
                                    ),
                                    collapsed,
                                    count: members.clone().count().to_string(),
                                },
                            });
                            if collapsed {
                                continue;
                            }
                            for external in members {
                                let (kind, children) = input.external_row(endpoint, external);
                                rows.push(SessionRow {
                                    endpoint_id: endpoint.endpoint_id.clone(),
                                    stale: endpoint.stale(),
                                    kind,
                                });
                                rows.extend(children.into_iter().map(|child| SessionRow {
                                    endpoint_id: endpoint.endpoint_id.clone(),
                                    stale: endpoint.stale(),
                                    kind: SessionRowKind::Activity(child, 0),
                                }));
                            }
                        }
                    }
                }
            }
        }
        SessionGrouping::Status => build_by_status(input, &mut rows),
    }
    let filtered = input
        .endpoints
        .iter()
        .find(|endpoint| &endpoint.endpoint_id == input.active_endpoint_id)
        .and_then(|endpoint| endpoint.snapshot.as_deref())
        .is_some_and(|snapshot| snapshot.agent_view_label.is_some());
    if filtered
        && !rows
            .iter()
            .any(|row| matches!(&row.kind, SessionRowKind::Agent(_)))
    {
        rows.insert(
            0,
            SessionRow {
                endpoint_id: input.active_endpoint_id.clone(),
                stale: false,
                kind: SessionRowKind::Empty(crate::i18n::texts().sidebar.no_matching_agents),
            },
        );
    }
    if rows.is_empty() {
        rows.push(SessionRow {
            endpoint_id: input.active_endpoint_id.clone(),
            stale: false,
            kind: SessionRowKind::Empty(
                if input.endpoints.iter().any(|endpoint| {
                    endpoint
                        .snapshot
                        .as_deref()
                        .is_some_and(|snapshot| snapshot.agent_view_label.is_some())
                }) {
                    crate::i18n::texts().sidebar.no_matching_agents
                } else {
                    match input.grouping {
                        SessionGrouping::Workspace => tr("No sessions yet", "还没有会话"),
                        SessionGrouping::Status => tr("No agent sessions", "没有智能体会话"),
                    }
                },
            ),
        });
    }
    (rows, rail, counts)
}

fn build_by_workspace(input: &BuildInput<'_>, rows: &mut Vec<SessionRow>) {
    let cached = cached_endpoint_snapshots(input.endpoints).collect::<Vec<_>>();
    let snapshot_of = |index: usize| {
        cached
            .iter()
            .copied()
            .find(|endpoint| endpoint.endpoint_index == index)
    };
    if input.endpoints.len() <= 1 {
        for endpoint in cached.iter().copied() {
            push_endpoint_sessions(input, endpoint, rows);
        }
        return;
    }
    let mut emitted: Vec<&str> = Vec::new();
    for (index, endpoint) in input.endpoints.iter().enumerate() {
        let Some(group) = input.machine_group(&endpoint.endpoint_id) else {
            push_machine(input, endpoint, snapshot_of(index), rows);
            continue;
        };
        if emitted.contains(&group) {
            continue;
        }
        emitted.push(group);
        rows.push(SessionRow {
            endpoint_id: endpoint.endpoint_id.clone(),
            stale: false,
            kind: SessionRowKind::MachineGroup {
                label: group.to_owned(),
            },
        });
        for (member_index, member) in input.endpoints.iter().enumerate() {
            if input.machine_group(&member.endpoint_id) == Some(group) {
                push_machine(input, member, snapshot_of(member_index), rows);
            }
        }
    }
}

fn push_machine(
    input: &BuildInput<'_>,
    endpoint: &ClientShellEndpoint,
    cached: Option<CachedEndpointSnapshot<'_>>,
    rows: &mut Vec<SessionRow>,
) {
    let collapsed = input
        .collapse
        .collapsed_endpoints
        .contains(&endpoint.endpoint_id);
    rows.push(SessionRow {
        endpoint_id: endpoint.endpoint_id.clone(),
        stale: false,
        kind: SessionRowKind::Machine {
            label: endpoint.label.clone(),
            status: endpoint.status,
            collapsed,
            rollup: cached.and_then(|cached| {
                Rollup::of(cached.snapshot.agents.iter().map(client_agent_bucket))
            }),
            tag: input.machine_tag(&endpoint.endpoint_id),
        },
    });
    if let Some(cached) = cached.filter(|_| !collapsed) {
        push_endpoint_sessions(input, cached, rows);
    }
}

fn push_endpoint_sessions(
    input: &BuildInput<'_>,
    endpoint: CachedEndpointSnapshot<'_>,
    rows: &mut Vec<SessionRow>,
) {
    let empty = HashSet::new();
    let snapshot = endpoint.snapshot;
    let stale = endpoint.stale();
    let endpoint_id = endpoint.endpoint_id;
    let push = |rows: &mut Vec<SessionRow>, kind| {
        rows.push(SessionRow {
            endpoint_id: endpoint_id.clone(),
            stale,
            kind,
        });
    };
    let groups = input.groups(endpoint_id).unwrap_or(&empty);
    let focused_pane = input.focused_pane(endpoint);
    for entry in super::super::sidebar::workspace_entries(snapshot, groups) {
        let Some(workspace) = snapshot.workspaces.get(entry.index) else {
            continue;
        };
        let parent_key = worktree_parent_key(snapshot, workspace);
        let toggle_key = parent_key
            .clone()
            .unwrap_or_else(|| agent_group_key(&workspace.workspace_id));
        let collapsed = groups.contains(&toggle_key);
        let group_member = |agent: &&ClientShellAgent| {
            agent.workspace_id == workspace.workspace_id
                || (collapsed
                    && parent_key.as_deref().is_some_and(|key| {
                        snapshot.workspaces.iter().any(|candidate| {
                            candidate.workspace_id == agent.workspace_id
                                && candidate
                                    .worktree
                                    .as_ref()
                                    .is_some_and(|worktree| worktree.key == key)
                        })
                    }))
        };
        let rollup = Rollup::of(
            snapshot
                .agents
                .iter()
                .filter(group_member)
                .map(client_agent_bucket),
        )
        .or_else(|| {
            if snapshot.agents.iter().any(|agent| group_member(&agent)) {
                return None;
            }
            Rollup::of(
                snapshot
                    .workspaces
                    .iter()
                    .filter(|candidate| {
                        candidate.workspace_id == workspace.workspace_id
                            || (collapsed
                                && parent_key.as_deref().is_some_and(|key| {
                                    candidate
                                        .worktree
                                        .as_ref()
                                        .is_some_and(|worktree| worktree.key == key)
                                }))
                    })
                    .map(|workspace| agent_bucket(workspace.agent_status)),
            )
            .map(|rollup| Rollup { count: 0, ..rollup })
        });
        let has_sessions = workspace_panes(snapshot, &workspace.workspace_id)
            .next()
            .is_some()
            || snapshot
                .agents
                .iter()
                .any(|agent| agent.workspace_id == workspace.workspace_id);
        let indent = if entry.indented { 2 } else { 0 };
        let holds_focus = collapsed
            && focused_pane.is_some_and(|pane_id| {
                snapshot.panes.iter().any(|pane| {
                    pane.pane_id == pane_id && pane.workspace_id == workspace.workspace_id
                })
            });
        push(
            rows,
            SessionRowKind::Workspace {
                workspace_id: workspace.workspace_id.clone(),
                label: workspace_label(workspace, entry.indented),
                meta: workspace_meta(workspace, entry.indented, &input.config.spaces),
                mobile_meta: workspace_mobile_meta(
                    snapshot,
                    workspace,
                    entry.indented,
                    &input.config.spaces,
                ),
                extras: workspace_extra_tokens(workspace, entry.indented, &input.config.spaces),
                focused: endpoint_id == input.active_endpoint_id && workspace.focused,
                indent,
                indented: entry.indented,
                last_child: entry.last_child,
                toggle: (has_sessions || parent_key.is_some())
                    .then(|| (toggle_key.clone(), collapsed)),
                rollup,
                holds_focus,
            },
        );
        if collapsed {
            continue;
        }
        let tabs = snapshot
            .tabs
            .iter()
            .filter(|tab| tab.workspace_id == workspace.workspace_id)
            .collect::<Vec<_>>();
        let panes = input.panes(endpoint, &workspace.workspace_id);
        let nested = tabs.len() > 1;
        let tab_groups = if nested {
            tabs.iter().map(|tab| Some(*tab)).collect::<Vec<_>>()
        } else {
            vec![None]
        };
        for tab in tab_groups {
            let child_indent = indent + u16::from(nested) * 2;
            if let Some(tab) = tab {
                let key = format!("agent-tab:{}", tab.tab_id);
                let collapsed = groups.contains(&key);
                let members = snapshot
                    .agents
                    .iter()
                    .filter(|agent| agent.tab_id == tab.tab_id)
                    .collect::<Vec<_>>();
                let bucket =
                    AgentBucket::rollup(members.iter().map(|agent| client_agent_bucket(agent)));
                push(
                    rows,
                    SessionRowKind::Tab {
                        tab_id: tab.tab_id.clone(),
                        label: tab.label.clone(),
                        key,
                        collapsed,
                        rollup: (!members.is_empty()).then_some(Rollup {
                            bucket,
                            count: members.len(),
                        }),
                        indent: child_indent,
                        holds_focus: collapsed
                            && focused_pane.is_some_and(|pane_id| {
                                snapshot.panes.iter().any(|pane| {
                                    pane.pane_id == pane_id && pane.tab_id == tab.tab_id
                                })
                            }),
                    },
                );
                if collapsed {
                    continue;
                }
            }
            for pane in panes
                .iter()
                .filter(|pane| tab.is_none_or(|tab| pane.tab_id == tab.tab_id))
            {
                if let Some(agent) = pane_agent(snapshot, &pane.pane_id) {
                    let (row, children) =
                        input.agent_row(endpoint, agent, child_indent, None, true);
                    push(rows, SessionRowKind::Agent(row));
                    for child in children {
                        push(rows, SessionRowKind::Activity(child, child_indent));
                    }
                } else {
                    push(
                        rows,
                        SessionRowKind::Terminal {
                            pane_id: pane.pane_id.clone(),
                            title: terminal_title(pane),
                            focused: focused_pane == Some(pane.pane_id.as_str()),
                            indent: child_indent,
                        },
                    );
                }
            }
            for ordered in input.ordered_agents.iter().filter(|ordered| {
                ordered.endpoint.endpoint_id == endpoint_id
                    && ordered.agent.workspace_id == workspace.workspace_id
                    && tab.is_none_or(|tab| ordered.agent.tab_id == tab.tab_id)
                    && !snapshot
                        .panes
                        .iter()
                        .any(|pane| pane.pane_id == ordered.agent.pane_id)
            }) {
                let (row, children) =
                    input.agent_row(endpoint, ordered.agent, child_indent, None, true);
                push(rows, SessionRowKind::Agent(row));
                for child in children {
                    push(rows, SessionRowKind::Activity(child, child_indent));
                }
            }
        }
    }
    for source in external_sources(&snapshot.external_agents) {
        let key = external_group_key(source);
        let collapsed = input.collapsed(endpoint_id, &key);
        let members = snapshot
            .external_agents
            .iter()
            .filter(|external| external.source == source);
        push(
            rows,
            SessionRowKind::ExternalGroup {
                label: format!(
                    "{} · {source}",
                    crate::i18n::texts().agent_panel.external_group
                ),
                collapsed,
                count: members.clone().count().to_string(),
                key,
            },
        );
        if collapsed {
            continue;
        }
        for external in members {
            let (row, children) = input.external_row(endpoint, external);
            push(rows, row);
            for child in children {
                push(rows, SessionRowKind::Activity(child, 0));
            }
        }
    }
}

fn build_by_status(input: &BuildInput<'_>, rows: &mut Vec<SessionRow>) {
    let federated = input.endpoints.len() > 1;
    let buckets = [
        AgentBucket::NeedsInput,
        AgentBucket::Failed,
        AgentBucket::Review,
        AgentBucket::Working,
        AgentBucket::Idle,
        AgentBucket::None,
    ];
    let mut sections: [Vec<SessionRow>; 6] = Default::default();
    let mut counts = [0usize; 6];
    let section_of = |bucket: AgentBucket| {
        buckets
            .iter()
            .position(|candidate| *candidate == bucket)
            .unwrap_or(5)
    };
    for endpoint in cached_endpoint_snapshots(input.endpoints) {
        let snapshot = endpoint.snapshot;
        let stale = endpoint.stale();
        let push = |section: &mut Vec<SessionRow>, kind| {
            section.push(SessionRow {
                endpoint_id: endpoint.endpoint_id.clone(),
                stale,
                kind,
            })
        };
        for workspace in &snapshot.workspaces {
            let location = if federated {
                format!("{}/{}", endpoint.label, workspace.label)
            } else {
                workspace.label.clone()
            };
            for pane in input.panes(endpoint, &workspace.workspace_id) {
                let (bucket, row, children) = if let Some(agent) =
                    pane_agent(snapshot, &pane.pane_id)
                {
                    let (row, children) =
                        input.agent_row(endpoint, agent, 0, Some(location.clone()), true);
                    (row.bucket, SessionRowKind::Agent(row), children)
                } else {
                    (
                        AgentBucket::None,
                        SessionRowKind::Terminal {
                            pane_id: pane.pane_id.clone(),
                            title: format!("{} · {location}", terminal_title(pane)),
                            focused: input.focused_pane(endpoint) == Some(pane.pane_id.as_str()),
                            indent: 0,
                        },
                        Vec::new(),
                    )
                };
                let section = section_of(bucket);
                counts[section] += 1;
                push(&mut sections[section], row);
                for child in children {
                    push(&mut sections[section], SessionRowKind::Activity(child, 0));
                }
            }
        }
        for external in &snapshot.external_agents {
            let section = section_of(agent_bucket(external.agent_status));
            let (row, children) = input.external_row(endpoint, external);
            counts[section] += 1;
            push(&mut sections[section], row);
            for child in children {
                push(&mut sections[section], SessionRowKind::Activity(child, 0));
            }
        }
    }
    for ((bucket, section), count) in buckets.into_iter().zip(sections).zip(counts) {
        if count == 0 {
            continue;
        }
        rows.push(SessionRow {
            endpoint_id: input.active_endpoint_id.clone(),
            stale: false,
            kind: SessionRowKind::Section {
                bucket,
                label: format!("{} · {count}", bucket_label(bucket)),
            },
        });
        rows.extend(section);
    }
}

pub(in crate::client::shell) fn attention_targets(
    endpoints: &[ClientShellEndpoint],
) -> Vec<(ClientEndpointId, String, AgentBucket)> {
    let mut targets = Vec::new();
    for endpoint in cached_endpoint_snapshots(endpoints).filter(|endpoint| !endpoint.stale()) {
        let snapshot = endpoint.snapshot;
        for workspace in &snapshot.workspaces {
            for pane in workspace_panes(snapshot, &workspace.workspace_id) {
                let Some(agent) = pane_agent(snapshot, &pane.pane_id) else {
                    continue;
                };
                let bucket = client_agent_bucket(agent);
                if bucket.wants_attention() {
                    targets.push((endpoint.endpoint_id.clone(), agent.pane_id.clone(), bucket));
                }
            }
        }
    }
    targets.sort_by_key(|(_, _, bucket)| bucket.listing_rank());
    targets
}

impl SessionRowsCache {
    fn build(key: SessionRowsKey, input: &BuildInput<'_>) -> Self {
        let (rows, rail, counts) = build_rows(input);
        let heights = rows.iter().map(SessionRow::height).collect::<Vec<_>>();
        let shows_ages = rows.iter().any(|row| match &row.kind {
            SessionRowKind::Agent(agent) => !agent.age.is_empty(),
            SessionRowKind::External { age, .. } => !age.is_empty(),
            _ => false,
        });
        let mut mobile_tail = Vec::new();
        if let Some(endpoint) = cached_endpoint_snapshots(input.endpoints)
            .find(|endpoint| endpoint.endpoint_id == input.active_endpoint_id)
        {
            let snapshot = endpoint.snapshot;
            let mut push = |label: String, target, focused| {
                mobile_tail.push(SessionRow {
                    endpoint_id: endpoint.endpoint_id.clone(),
                    stale: endpoint.stale(),
                    kind: SessionRowKind::MobileAction {
                        label,
                        target,
                        focused,
                    },
                })
            };
            push(
                crate::i18n::texts().mobile.new_workspace.into(),
                Some(ClientMobileTarget::NewWorkspace),
                false,
            );
            if let Some(workspace) = snapshot.focused_workspace_id.as_deref() {
                push(crate::i18n::texts().mobile.section_tabs.into(), None, false);
                push(
                    crate::i18n::texts().mobile.new_tab.into(),
                    Some(ClientMobileTarget::NewTab),
                    false,
                );
                for (index, tab) in snapshot
                    .tabs
                    .iter()
                    .filter(|tab| tab.workspace_id == workspace)
                    .enumerate()
                {
                    push(
                        if tab.custom_label {
                            format!("{} · {}", index + 1, tab.label)
                        } else {
                            crate::i18n::fill(
                                crate::i18n::texts().mobile.tab_label_fmt,
                                &[("label", &tab.label)],
                            )
                        },
                        Some(ClientMobileTarget::Tab {
                            endpoint_id: endpoint.endpoint_id.clone(),
                            tab_id: tab.tab_id.clone(),
                        }),
                        tab.focused,
                    );
                }
            }
            push(crate::i18n::texts().mobile.section_menu.into(), None, false);
            for (index, item) in super::super::global_menu::global_menu_items(snapshot)
                .into_iter()
                .enumerate()
            {
                let suffix = if item.enabled {
                    ""
                } else {
                    crate::i18n::texts().menu.unavailable_suffix
                };
                push(
                    format!("{}{suffix}", item.label),
                    Some(ClientMobileTarget::Menu(index)),
                    false,
                );
            }
        }
        let mobile_heights = rows
            .iter()
            .map(|row| row.height_for(true))
            .chain(mobile_tail.iter().map(SessionRow::height))
            .collect();
        let gaps = (0..rows.len())
            .map(|index| {
                if !matches!(
                    rows[index].kind,
                    SessionRowKind::Machine { .. }
                        | SessionRowKind::MachineGroup { .. }
                        | SessionRowKind::Section { .. }
                ) && rows.get(index + 1).is_some_and(|row| {
                    row.endpoint_id == rows[index].endpoint_id
                        && matches!(
                            row.kind,
                            SessionRowKind::Workspace {
                                indented: false,
                                ..
                            }
                        )
                }) {
                    input.config.spaces.row_gap
                } else if index + 1 < rows.len()
                    && matches!(
                        rows[index].kind,
                        SessionRowKind::Agent(_)
                            | SessionRowKind::Tab { .. }
                            | SessionRowKind::Activity(..)
                            | SessionRowKind::External { .. }
                    )
                {
                    input.config.agents.row_gap
                } else {
                    0
                }
            })
            .collect::<Vec<_>>();
        let mobile_gaps = gaps
            .iter()
            .copied()
            .chain(std::iter::repeat_n(0, mobile_tail.len()))
            .collect();
        Self {
            key,
            rows,
            heights,
            rail,
            counts,
            shows_ages,
            mobile_tail,
            mobile_heights,
            gaps,
            mobile_gaps,
        }
    }

    fn displayed_rows(&self, mobile: bool) -> impl Iterator<Item = &SessionRow> {
        self.rows.iter().chain(if mobile {
            self.mobile_tail.as_slice()
        } else {
            &[]
        })
    }

    pub(in crate::client::shell) fn ages_expired(&self, now_ms: u64) -> bool {
        self.shows_ages && now_ms / 60_000 != self.key.minute
    }
}

impl SessionRowsKey {
    pub(in crate::client::shell) fn for_state(state: &ClientShellState) -> Self {
        Self {
            config_epoch: state.config_epoch,
            data_epoch: state.agent_rows_epoch,
            collapse_epoch: state.tree_collapse_epoch,
            activity_epoch: state.agent_activity_epoch,
            active_endpoint: state.active_endpoint_id.clone(),
            grouping: state.session_grouping,
            sort: state.config.agent_panel_sort,
            minute: state.wall_clock_ms() / 60_000,
            endpoints: state
                .endpoints
                .iter()
                .map(|endpoint| {
                    (
                        endpoint.endpoint_id.clone(),
                        endpoint
                            .snapshot
                            .as_deref()
                            .map(|snapshot| (snapshot.revision, snapshot.boot_id.clone())),
                        endpoint.label.clone(),
                        endpoint.status,
                    )
                })
                .collect(),
        }
    }
}

impl SessionRowsCache {
    pub(in crate::client::shell) fn key_matches(&self, key: &SessionRowsKey) -> bool {
        self.key == *key
    }
}

impl ClientShellState {
    pub(in crate::client::shell) fn wall_clock_ms(&self) -> u64 {
        self.wall_clock_override_ms.unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| {
                    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
                })
        })
    }

    pub(in crate::client::shell) fn refresh_session_rows(&mut self) {
        let now_ms = self.wall_clock_ms();
        let key = SessionRowsKey::for_state(self);
        if self
            .session_rows
            .as_ref()
            .is_some_and(|cache| cache.key_matches(&key))
        {
            return;
        }
        let collapse = CollapseState {
            collapsed_groups: &self.collapsed_groups,
            remote_collapsed_groups: &self.remote_collapsed_groups,
            collapsed_endpoints: &self.collapsed_endpoints,
        };
        let input = BuildInput {
            endpoints: &self.endpoints,
            active_endpoint_id: &self.active_endpoint_id,
            config: &self.config,
            collapse: &collapse,
            machine_chrome: &self.machine_chrome,
            grouping: self.session_grouping,
            now_ms,
            ordered_agents: super::super::aggregate_navigation::aggregate_agent_rows(
                &self.endpoints,
                &self.active_endpoint_id,
                self.config.agent_panel_sort,
            ),
        };
        self.session_rows = Some(SessionRowsCache::build(key, &input));
    }

    pub(in crate::client::shell) fn workbench_geometry(&self, cols: u16, rows: u16) -> Geometry {
        self.workbench.geometry(cols, rows)
    }

    pub(in crate::client::shell) fn compute_session_view(&mut self, cols: u16, rows: u16) {
        if let Some((endpoint_id, pane_id)) = self.reveal_session_target.as_ref() {
            let keys = self
                .endpoints
                .iter()
                .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
                .and_then(|endpoint| endpoint.snapshot.as_deref())
                .and_then(|snapshot| {
                    let agent = snapshot
                        .agents
                        .iter()
                        .find(|agent| &agent.pane_id == pane_id)?;
                    let workspace = snapshot
                        .workspaces
                        .iter()
                        .find(|workspace| workspace.workspace_id == agent.workspace_id)?;
                    Some([
                        Some(format!("agent-tab:{}", agent.tab_id)),
                        Some(agent_group_key(&workspace.workspace_id)),
                        worktree_parent_key(snapshot, workspace),
                        workspace
                            .worktree
                            .as_ref()
                            .map(|worktree| worktree.key.clone()),
                    ])
                });
            if let Some(keys) = keys {
                let mut changed = self.collapsed_endpoints.remove(endpoint_id);
                let groups = if endpoint_id.is_local() {
                    Some(&mut self.collapsed_groups)
                } else {
                    self.remote_collapsed_groups.get_mut(endpoint_id)
                };
                if let Some(groups) = groups {
                    for key in keys.into_iter().flatten() {
                        changed |= groups.remove(&key);
                    }
                }
                if changed {
                    self.bump_tree_collapse_epoch();
                }
            }
        }
        self.refresh_session_rows();
        if !self.workbench.enabled
            && self
                .snapshot
                .as_deref()
                .zip(self.pane_surface.as_ref())
                .is_some_and(|(snapshot, surface)| {
                    self.pending_pane_surface.is_some()
                        || self.pane_surface_generation != self.active_snapshot_generation
                        || snapshot.revision != surface.projection_revision
                })
        {
            return;
        }
        let mut view = SessionView::default();
        if self.workbench.enabled {
            self.workbench.geometry = self.workbench_geometry(cols, rows);
            let geometry = &self.workbench.geometry;
            if let Some((_, area)) = geometry
                .panels
                .iter()
                .find(|(panel, _)| *panel == PanelId::Workspaces)
            {
                view.rail = geometry.rail;
                view.area = if geometry.rail {
                    *area
                } else {
                    super::super::workbench::body(*area, &PanelId::Workspaces, geometry.compact)
                };
            }
        } else {
            let layout = self.layout(cols, rows);
            view.mobile = !layout.mobile_header.is_empty()
                && self.mode == ClientShellMode::Navigate
                && self.overlay.is_none();
            if view.mobile {
                view.area = Rect::new(0, 3.min(rows), cols, rows.saturating_sub(3));
                if view.area.is_empty() {
                    self.mobile_switcher_scroll = 0;
                    self.reveal_mobile_workspace = false;
                }
            } else if !layout.sidebar.is_empty()
                || self.snapshot.is_none()
                || self.pane_surface.is_none()
            {
                let area = if !layout.sidebar.is_empty() {
                    layout.sidebar
                } else {
                    Rect::new(0, 5.min(rows), cols, rows.saturating_sub(6))
                };
                view.sidebar_divider =
                    Rect::new(area.right().saturating_sub(1), area.y, 1, area.height);
                view.sidebar_toggle =
                    super::super::endpoint_sidebar::sidebar_toggle_rect(area, false);
                if self.sidebar_collapsed && !layout.sidebar.is_empty() {
                    view.area =
                        Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1));
                    view.rail = true;
                } else {
                    let content =
                        Rect::new(area.x, area.y, area.width.saturating_sub(1), area.height);
                    view.header = Rect::new(
                        content.x,
                        content.y,
                        content.width,
                        u16::from(content.height > 0),
                    );
                    view.area = Rect::new(
                        content.x,
                        content.y.saturating_add(1),
                        content.width,
                        content.height.saturating_sub(1),
                    );
                }
            }
        }
        view.selected = self.session_selection();
        if let Some(ClientChromeDrag::Workspace {
            source_workspace_id,
            target,
        }) = &self.chrome_drag
        {
            view.dragged = Some(source_workspace_id.clone());
            view.drop_row = target.as_ref().map(|(_, row)| *row);
        }
        if !view.mobile && !view.area.is_empty() && view.area.width < RAIL_BELOW_PANEL {
            view.rail = true;
        }
        self.layout_session_view(&mut view);
        self.place_drop_indicator(&mut view);
        self.session_view = view;
    }

    fn place_drop_indicator(&self, view: &mut SessionView) {
        let (Some(target), Some(cache)) = (view.drop_row, self.session_rows.as_ref()) else {
            return;
        };
        if view.rail {
            return;
        }
        let mut y = view.body.y;
        let mut group_end: Option<u16> = None;
        for (row, height) in cache.rows.iter().zip(&cache.heights).skip(view.start) {
            if y.saturating_add(*height) > view.body.bottom() {
                break;
            }
            let bottom = y.saturating_add(*height);
            match (&row.kind, group_end) {
                (SessionRowKind::Workspace { .. }, _)
                    if bottom == target && row.endpoint_id == self.active_endpoint_id =>
                {
                    group_end = Some(bottom);
                }
                (
                    SessionRowKind::Agent(_)
                    | SessionRowKind::Terminal { .. }
                    | SessionRowKind::Activity(..),
                    Some(_),
                ) => group_end = Some(bottom),
                (_, Some(_)) => break,
                _ => {}
            }
            y = bottom;
        }
        if let Some(end) = group_end.filter(|end| *end > target) {
            view.drop_row = Some(end.saturating_sub(1));
        }
    }

    fn session_selection(&self) -> Option<WorkspaceNavigationTarget> {
        let navigating = self.mode == ClientShellMode::Navigate;
        self.navigate_workspace_id
            .as_ref()
            .filter(|target| navigating && self.navigation_target_valid(target))
            .or_else(|| {
                self.pending_workspace_highlight
                    .as_ref()
                    .filter(|pending| {
                        !navigating
                            && pending.target.endpoint_id == self.active_endpoint_id
                            && self.navigation_target_valid(&pending.target)
                    })
                    .map(|pending| &pending.target)
            })
            .cloned()
    }

    fn layout_session_view(&mut self, view: &mut SessionView) {
        let area = view.area;
        if area.is_empty() {
            return;
        }
        let Some(cache) = self.session_rows.as_ref() else {
            return;
        };
        let reveal_pane = self.reveal_session_target.take();
        let pane_workspace = reveal_pane.as_ref().and_then(|(endpoint_id, pane_id)| {
            self.endpoints
                .iter()
                .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
                .and_then(|endpoint| endpoint.snapshot.as_deref())
                .and_then(|snapshot| snapshot.panes.iter().find(|pane| &pane.pane_id == pane_id))
                .and_then(|pane| self.navigation_target(endpoint_id, &pane.workspace_id))
        });
        if view.rail {
            let chrome = u16::from(area.height >= 5);
            view.overview = Rect::new(area.x, area.y, area.width, chrome);
            view.footer = Rect::new(
                area.x,
                area.bottom().saturating_sub(chrome),
                area.width,
                chrome,
            );
            view.body = Rect::new(
                area.x,
                area.y.saturating_add(chrome),
                area.width,
                area.height.saturating_sub(chrome * 2),
            );
            let mut start = self.workspace_scroll;
            let reveal_nav = std::mem::take(&mut self.reveal_navigation_workspace);
            let reveal_focus = std::mem::take(&mut self.reveal_focused_workspace);
            let target = if reveal_nav {
                view.selected.as_ref().and_then(|target| {
                    cache
                        .rail
                        .iter()
                        .position(|item| target.matches(&item.endpoint_id, &item.workspace_id))
                })
            } else {
                None
            }
            .or_else(|| {
                pane_workspace.as_ref().and_then(|target| {
                    cache
                        .rail
                        .iter()
                        .position(|item| target.matches(&item.endpoint_id, &item.workspace_id))
                })
            })
            .or_else(|| {
                reveal_focus
                    .then(|| cache.rail.iter().position(|item| item.focused))
                    .flatten()
            });
            if let Some(target) = target {
                start = super::super::scroll::uniform_scroll_start_to_reveal(
                    cache.rail.len(),
                    view.body.height,
                    start,
                    target,
                );
            }
            let metrics = super::super::scroll::uniform_scroll_metrics(
                cache.rail.len(),
                view.body.height,
                start,
            );
            view.start = metrics
                .max_offset_from_bottom
                .saturating_sub(metrics.offset_from_bottom);
            view.metrics = Some(metrics);
            self.workspace_scroll = view.start;
            return;
        }
        let mut y = area.y;
        let mut bottom = area.bottom();
        if area.height >= 3 {
            view.footer = Rect::new(area.x, bottom - 1, area.width, 1);
            bottom -= 1;
        }
        if area.height >= 8 {
            view.overview = Rect::new(area.x, y, area.width, 1);
            view.grouping = Rect::new(area.x, y + 1, area.width, 1);
            y += 2;
        }
        view.body = Rect::new(area.x, y, area.width, bottom.saturating_sub(y));
        let gaps = if view.mobile {
            &cache.mobile_gaps
        } else {
            &cache.gaps
        };
        let heights = if view.mobile {
            &cache.mobile_heights
        } else {
            &cache.heights
        };
        let mut start = if view.mobile {
            self.mobile_switcher_scroll
        } else {
            self.workspace_scroll
        };
        let reveal_nav = if view.mobile {
            std::mem::take(&mut self.reveal_mobile_workspace)
        } else {
            std::mem::take(&mut self.reveal_navigation_workspace)
        };
        let reveal_focus = !view.mobile && std::mem::take(&mut self.reveal_focused_workspace);
        let pane_matches_workspace = self.snapshot.as_deref().is_none_or(|snapshot| {
            snapshot
                .panes
                .iter()
                .find(|pane| Some(pane.pane_id.as_str()) == snapshot.focused_pane_id.as_deref())
                .is_none_or(|pane| {
                    Some(pane.workspace_id.as_str()) == snapshot.focused_workspace_id.as_deref()
                })
        });
        let workspace_row = |target: &WorkspaceNavigationTarget| {
            cache.rows.iter().position(|row| {
            matches!(&row.kind, SessionRowKind::Workspace { workspace_id, .. } if target.matches(&row.endpoint_id, workspace_id))
        })
        };
        let target = if reveal_nav {
            view.selected.as_ref().and_then(workspace_row)
        } else {
            None
        }
        .or_else(|| {
            reveal_pane
                .as_ref()
                .and_then(|(endpoint, pane)| {
                    cache.rows.iter().position(|row| {
                        &row.endpoint_id == endpoint
                            && match &row.kind {
                                SessionRowKind::Agent(agent) => &agent.pane_id == pane,
                                SessionRowKind::Terminal { pane_id, .. } => pane_id == pane,
                                _ => false,
                            }
                    })
                })
                .or_else(|| pane_workspace.as_ref().and_then(workspace_row))
        })
        .or_else(|| {
            reveal_focus
                .then(|| {
                    cache
                        .rows
                        .iter()
                        .position(|row| match &row.kind {
                            SessionRowKind::Agent(agent) => agent.focused && pane_matches_workspace,
                            SessionRowKind::Terminal { focused, .. } => {
                                *focused && pane_matches_workspace
                            }
                            SessionRowKind::Workspace { holds_focus, .. } => *holds_focus,
                            _ => false,
                        })
                        .or_else(|| {
                            cache.rows.iter().position(|row| {
                                matches!(row.kind, SessionRowKind::Workspace { focused: true, .. })
                            })
                        })
                })
                .flatten()
        });
        if !view.body.is_empty() {
            if let Some(target) = target {
                start = super::super::scroll::list_scroll_start_to_reveal(
                    heights,
                    gaps,
                    view.body.height,
                    start,
                    target,
                );
            }
        }
        let metrics =
            super::super::scroll::list_scroll_metrics(heights, gaps, view.body.height, start);
        view.start = metrics
            .max_offset_from_bottom
            .saturating_sub(metrics.offset_from_bottom);
        view.metrics = Some(metrics);
        if view.mobile {
            self.mobile_switcher_scroll = view.start;
        } else {
            self.workspace_scroll = view.start;
        }
    }

    pub(in crate::client::shell) fn handle_session_click(
        &mut self,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(hit) = self
            .hits
            .sessions
            .iter()
            .rev()
            .find(|(rect, _)| super::super::contains(*rect, point))
            .map(|(_, hit)| hit.clone())
        else {
            return false;
        };
        use crate::input::{KeybindAction, KeybindMatch};
        match hit {
            SessionHit::NewWorkspace => {
                self.record_binding(KeybindMatch::Action(KeybindAction::NewWorkspace), outcome)
            }
            SessionHit::Search | SessionHit::Overview => {
                self.record_binding(KeybindMatch::Action(KeybindAction::OpenNavigator), outcome)
            }
            SessionHit::Grouping => self.toggle_session_grouping(outcome),
            SessionHit::Toggle(endpoint_id, key) => {
                self.toggle_collapsed_group(&endpoint_id, key);
                self.persist_chrome_preferences(outcome);
                outcome.repaint = true;
            }
            SessionHit::Terminal(endpoint_id, pane_id) => {
                self.focus_agent_pane(endpoint_id, pane_id, outcome);
            }
            SessionHit::Tab(endpoint_id, tab_id) => {
                self.focus_or_activate(
                    endpoint_id,
                    ClientEndpointFocusTarget::Tab(tab_id),
                    outcome,
                );
                outcome.repaint = true;
            }
            SessionHit::Settings => {
                self.record_binding(KeybindMatch::Action(KeybindAction::Settings), outcome)
            }
            SessionHit::Help => {
                self.record_binding(KeybindMatch::Action(KeybindAction::Help), outcome)
            }
            SessionHit::Machines => {
                self.record_binding(KeybindMatch::Action(KeybindAction::ManageMachines), outcome)
            }
            SessionHit::Rail(endpoint_id, workspace_id) => self.finish_endpoint_workspace_press(
                ClientWorkspacePress {
                    endpoint_id,
                    workspace_id,
                    start_row: point.1,
                },
                outcome,
            ),
        }
        true
    }

    pub(in crate::client::shell) fn toggle_session_grouping(
        &mut self,
        outcome: &mut ClientShellInput,
    ) {
        self.session_grouping = self.session_grouping.toggled();
        self.session_grouping_manual = true;
        self.workspace_scroll = 0;
        self.persist_chrome_preferences(outcome);
        outcome.repaint = true;
    }

    pub(in crate::client::shell) fn focus_next_attention(
        &mut self,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let targets = attention_targets(&self.endpoints);
        if targets.is_empty() {
            return false;
        }
        let focused = self
            .snapshot
            .as_deref()
            .and_then(|snapshot| snapshot.focused_pane_id.as_deref());
        let next = targets
            .iter()
            .position(|(endpoint_id, pane_id, _)| {
                endpoint_id == &self.active_endpoint_id && Some(pane_id.as_str()) == focused
            })
            .map_or(0, |index| (index + 1) % targets.len());
        let (endpoint_id, pane_id, _) = targets[next].clone();
        self.focus_agent_pane(endpoint_id, pane_id, outcome);
        outcome.repaint = true;
        true
    }
}

pub(in crate::client::shell) struct SessionPaint<'a> {
    pub(in crate::client::shell) cache: &'a SessionRowsCache,
    pub(in crate::client::shell) view: &'a SessionView,
    pub(in crate::client::shell) grouping: SessionGrouping,
    pub(in crate::client::shell) config: &'a ClientShellConfig,
    pub(in crate::client::shell) glyphs: StatusGlyphs,
    pub(in crate::client::shell) hover: Option<&'a ChromeHover>,
    pub(in crate::client::shell) spinner: &'a str,
    pub(in crate::client::shell) endpoints: &'a [ClientShellEndpoint],
    pub(in crate::client::shell) active_endpoint_id: &'a ClientEndpointId,
}

impl SessionPaint<'_> {
    fn hovered(&self, hit: &SessionHit) -> bool {
        matches!(self.hover, Some(ChromeHover::Session(current)) if current == hit)
    }
    fn glyphs_for(&self, stale: bool) -> StatusGlyphs {
        if stale {
            StatusGlyphs::fixed(self.config.status_indicators)
        } else {
            self.glyphs
        }
    }
}

pub(in crate::client::shell) fn paint_sessions(
    buffer: &mut Buffer,
    frame: &SessionPaint<'_>,
    hits: &mut ShellHitMap,
) {
    let view = frame.view;
    if view.area.is_empty() {
        return;
    }
    let palette = &frame.config.palette;
    buffer.set_style(view.area, Style::default().bg(palette.sidebar_bg));
    if view.rail {
        paint_rail(buffer, frame, hits);
        return;
    }
    paint_overview(buffer, frame, hits);
    paint_grouping(buffer, frame, hits);
    paint_body(buffer, frame, hits);
    paint_footer(buffer, frame, hits);
    hits.session_agents.append(&mut hits.endpoint_agents);
    hits.session_tree_toggles
        .append(&mut hits.agent_tree_toggles);
    hits.session_activity_rows
        .append(&mut hits.agent_activity_rows);
    hits.session_external_agents
        .append(&mut hits.external_agents);
}

fn hover_bg(frame: &SessionPaint<'_>, rect: Rect, buffer: &mut Buffer, hovered: bool) {
    if hovered {
        buffer.set_style(
            rect,
            Style::default().bg(frame.config.palette.hover_row_bg()),
        );
    }
}

fn paint_overview(buffer: &mut Buffer, frame: &SessionPaint<'_>, hits: &mut ShellHitMap) {
    let rect = frame.view.overview;
    if rect.is_empty() {
        return;
    }
    let palette = &frame.config.palette;
    let hovered = frame.hovered(&SessionHit::Overview);
    hover_bg(frame, rect, buffer, hovered);
    let counts = frame.cache.counts;
    let mut chips = Vec::new();
    for bucket in super::title_bar::CHIP_ORDER {
        let count = counts.get(bucket);
        if count == 0 {
            continue;
        }
        if !chips.is_empty() {
            chips.push(Span::new(" ", Style::default()));
        }
        chips.push(Span::new(
            frame.glyphs.bucket(bucket),
            crate::ui::design::status_style(bucket, palette, &frame.config.components),
        ));
        chips.push(Span::new(
            count.to_string(),
            Style::default().fg(crate::ui::design::status_color(
                bucket,
                palette,
                &frame.config.components,
            )),
        ));
    }
    let chips_width = super::spans_width(&chips);
    let right = rect.right().saturating_sub(INSET);
    let chips_x = right.saturating_sub(chips_width);
    let label_style = if hovered {
        crate::ui::design::title(palette)
    } else {
        crate::ui::design::body(palette)
    };
    let label_x = rect.x.saturating_add(INSET).saturating_add(2);
    let label_budget = chips_x.saturating_sub(1).saturating_sub(label_x);
    let x = paint_spans(
        buffer,
        rect.x.saturating_add(INSET),
        rect.y,
        chips_x.saturating_sub(1).max(rect.x),
        &[
            Span::new("◎ ", Style::default().fg(palette.subtext0)),
            Span::new(
                truncate_end(
                    tr("Agent overview", "智能体总览"),
                    usize::from(label_budget),
                ),
                label_style,
            ),
        ],
    );
    if chips_x > x {
        paint_spans(buffer, chips_x, rect.y, right, &chips);
    }
    hits.sessions.push((rect, SessionHit::Overview));
}

fn paint_grouping(buffer: &mut Buffer, frame: &SessionPaint<'_>, hits: &mut ShellHitMap) {
    let rect = frame.view.grouping;
    if rect.is_empty() {
        return;
    }
    let palette = &frame.config.palette;
    let toggle = frame.grouping.toggle_label();
    let toggle_width = display_width_u16(toggle);
    let right = rect.right().saturating_sub(INSET);
    let toggle_rect = Rect::new(
        right.saturating_sub(toggle_width).max(rect.x),
        rect.y,
        toggle_width.min(rect.width),
        1,
    );
    let label_x = rect.x.saturating_add(INSET);
    let label_budget = toggle_rect.x.saturating_sub(1).saturating_sub(label_x);
    let custom = frame
        .endpoints
        .iter()
        .find(|endpoint| &endpoint.endpoint_id == frame.active_endpoint_id)
        .and_then(|endpoint| endpoint.snapshot.as_deref())
        .and_then(|snapshot| snapshot.agent_view_label.as_deref());
    let label = custom.unwrap_or(
        if frame.config.agent_panel_sort == crate::config::AgentPanelSortConfig::Launch {
            tr("Launch ↑", "启动 ↑")
        } else {
            frame.grouping.section_label()
        },
    );
    let label = truncate_end(label, usize::from(label_budget));
    if custom.is_none() && !label.is_empty() {
        hits.agent_sort_toggle = Rect::new(label_x, rect.y, display_width_u16(&label), 1);
    }
    paint_spans(
        buffer,
        label_x,
        rect.y,
        toggle_rect.x.saturating_sub(1),
        &[Span::new(label, crate::ui::design::section(palette))],
    );
    let hovered = frame.hovered(&SessionHit::Grouping);
    let style = if hovered {
        Style::default().fg(palette.text).bg(palette.hover_row_bg())
    } else {
        crate::ui::design::meta(palette)
    };
    paint_spans(
        buffer,
        toggle_rect.x,
        rect.y,
        right,
        &[Span::new(toggle, style)],
    );
    hits.sessions.push((toggle_rect, SessionHit::Grouping));
}

fn paint_footer(buffer: &mut Buffer, frame: &SessionPaint<'_>, hits: &mut ShellHitMap) {
    let rect = frame.view.footer;
    if rect.is_empty() {
        return;
    }
    let palette = &frame.config.palette;
    let item = |hit: &SessionHit| {
        if frame.hovered(hit) {
            Style::default().fg(palette.text).bg(palette.hover_row_bg())
        } else {
            crate::ui::design::meta(palette)
        }
    };
    let active = frame
        .endpoints
        .iter()
        .find(|endpoint| &endpoint.endpoint_id == frame.active_endpoint_id);
    let (glyph, _, color) = super::super::endpoints::endpoint_status_presentation(
        active.map_or(ClientEndpointStatus::Online, |endpoint| endpoint.status),
        palette,
        frame.spinner,
    );
    let mut machine = active.map_or_else(
        || crate::i18n::texts().sidebar.local.to_owned(),
        |endpoint| endpoint.label.clone(),
    );
    if frame.endpoints.len() > 1 {
        machine.push_str(&format!(" +{}", frame.endpoints.len() - 1));
    }
    let machine_spans = [
        Span::new(glyph, Style::default().fg(color)),
        Span::new(format!(" {machine}"), item(&SessionHit::Machines)),
    ];
    let machine_width = super::spans_width(&machine_spans).min(rect.width.saturating_sub(INSET));
    let right = rect.right().saturating_sub(INSET);
    let machine_rect = Rect::new(
        right.saturating_sub(machine_width),
        rect.y,
        machine_width,
        1,
    );
    let start = rect.x.saturating_add(INSET);
    let limit = machine_rect.x.saturating_sub(1);
    let full = [
        (SessionHit::Settings, tr("⚙ Settings", "⚙ 设置")),
        (SessionHit::Help, tr("? Help", "? 帮助")),
    ];
    let compact = [(SessionHit::Settings, "⚙"), (SessionHit::Help, "?")];
    let width_of = |items: &[(SessionHit, &str)]| {
        items
            .iter()
            .map(|(_, label)| display_width_u16(label).saturating_add(2))
            .fold(0u16, u16::saturating_add)
            .saturating_sub(2)
    };
    let items: &[(SessionHit, &str)] = if start.saturating_add(width_of(&full)) <= limit {
        &full
    } else {
        &compact
    };
    let mut x = start;
    for (hit, label) in items {
        let width = display_width_u16(label);
        if x.saturating_add(width) > limit {
            break;
        }
        paint_spans(buffer, x, rect.y, limit, &[Span::new(*label, item(hit))]);
        hits.sessions
            .push((Rect::new(x, rect.y, width, 1), hit.clone()));
        x = x.saturating_add(width).saturating_add(2);
    }
    if machine_width > 0 {
        paint_spans(buffer, machine_rect.x, rect.y, right, &machine_spans);
        hits.sessions.push((machine_rect, SessionHit::Machines));
    }
}

fn paint_rail(buffer: &mut Buffer, frame: &SessionPaint<'_>, hits: &mut ShellHitMap) {
    let view = frame.view;
    let palette = &frame.config.palette;
    let area = view.area;
    let rule = Rect::new(area.right().saturating_sub(1), area.y, 1, area.height);
    for y in rule.y..rule.bottom() {
        buffer[(rule.x, y)]
            .set_symbol("│")
            .set_style(crate::ui::design::rule(palette));
    }
    let cell = |rect: Rect| Rect::new(rect.x, rect.y, rect.width.saturating_sub(1), rect.height);
    if !view.overview.is_empty() {
        let rect = cell(view.overview);
        hover_bg(frame, rect, buffer, frame.hovered(&SessionHit::Overview));
        let attention = frame.cache.counts.needs_input + frame.cache.counts.failed > 0;
        paint_spans(
            buffer,
            rect.x.saturating_add(1),
            rect.y,
            rect.right(),
            &[Span::new(
                "◎",
                Style::default().fg(if attention {
                    palette.text
                } else {
                    palette.subtext0
                }),
            )],
        );
        hits.sessions.push((rect, SessionHit::Overview));
    }
    if !view.footer.is_empty() {
        let rect = cell(view.footer);
        hover_bg(frame, rect, buffer, frame.hovered(&SessionHit::Settings));
        paint_spans(
            buffer,
            rect.x.saturating_add(1),
            rect.y,
            rect.right(),
            &[Span::new("⚙", crate::ui::design::meta(palette))],
        );
        hits.sessions.push((rect, SessionHit::Settings));
    }
    let body = view.body;
    hits.workspace_body = body;
    if let Some(metrics) = view.metrics {
        hits.workspace_max_scroll = metrics.max_offset_from_bottom;
    }
    for (index, item) in frame
        .cache
        .rail
        .iter()
        .skip(view.start)
        .take(usize::from(body.height))
        .enumerate()
    {
        let rect = cell(Rect::new(body.x, body.y + index as u16, body.width, 1));
        if let Some((label, status, tag, collapsed)) = &item.machine {
            let color = match tag {
                Some(MachineTag::Color(color)) => *color,
                Some(MachineTag::Hashed) => {
                    super::super::machines_overlay::machine_hash_color(label, palette)
                }
                None => palette.text,
            };
            paint_spans(
                buffer,
                rect.x,
                rect.y,
                rect.right(),
                &[
                    Span::new(
                        if *collapsed { "▸" } else { "▾" },
                        crate::ui::design::meta(palette),
                    ),
                    Span::new(
                        item.number.as_str(),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                ],
            );
            if !item.endpoint_id.is_local() {
                let (glyph, _, color) = super::super::endpoints::endpoint_status_presentation(
                    *status,
                    palette,
                    frame.spinner,
                );
                paint_spans(
                    buffer,
                    rect.right().saturating_sub(1),
                    rect.y,
                    rect.right(),
                    &[Span::new(glyph, Style::default().fg(color))],
                );
            }
            hits.machines.push(MachineHit {
                rect,
                collapse_toggle: Rect::new(rect.x, rect.y, 1, 1),
                endpoint_id: item.endpoint_id.clone(),
            });
            continue;
        }
        let hit = SessionHit::Rail(item.endpoint_id.clone(), item.workspace_id.clone());
        let selected = view
            .selected
            .as_ref()
            .is_some_and(|target| target.matches(&item.endpoint_id, &item.workspace_id));
        if selected {
            buffer.set_style(rect, Style::default().bg(palette.selection_row_bg()));
        } else if item.focused {
            buffer.set_style(rect, Style::default().bg(palette.active_row_bg));
        } else {
            hover_bg(frame, rect, buffer, frame.hovered(&hit));
        }
        let number_style = Style::default()
            .fg(if item.focused {
                palette.text
            } else {
                palette.overlay0
            })
            .add_modifier(if item.focused {
                Modifier::BOLD
            } else {
                Modifier::empty()
            });
        let glyph_bucket = if item.bucket == AgentBucket::None {
            AgentBucket::Idle
        } else {
            item.bucket
        };
        let glyph_style = if item.stale {
            Style::default()
                .fg(palette.overlay0)
                .add_modifier(Modifier::DIM)
        } else {
            crate::ui::design::status_style(glyph_bucket, palette, &frame.config.components)
        };
        let number = if display_width_u16(&item.number) > 2 {
            "…"
        } else {
            item.number.as_str()
        };
        let pad = 2u16.saturating_sub(display_width_u16(number));
        paint_spans(
            buffer,
            rect.x.saturating_add(pad),
            rect.y,
            rect.right(),
            &[
                Span::new(number, number_style),
                Span::new(
                    frame.glyphs_for(item.stale).bucket(glyph_bucket),
                    glyph_style,
                ),
            ],
        );
        hits.workspaces.push(WorkspaceHit {
            rect,
            endpoint_id: item.endpoint_id.clone(),
            workspace_id: item.workspace_id.clone(),
            indented: false,
            group_toggle: None,
        });
        if view.sidebar_toggle.is_empty() {
            hits.sessions.push((rect, hit));
        }
    }
}

fn paint_body(buffer: &mut Buffer, frame: &SessionPaint<'_>, hits: &mut ShellHitMap) {
    let view = frame.view;
    let body = view.body;
    hits.workspace_body = body;
    hits.new_workspace = Rect::new(body.x, body.bottom(), 0, u16::from(!body.is_empty()));
    if body.is_empty() {
        return;
    }
    let palette = &frame.config.palette;
    let metrics = view.metrics.unwrap_or(crate::pane::ScrollMetrics {
        offset_from_bottom: 0,
        max_offset_from_bottom: 0,
        viewport_rows: 0,
    });
    hits.workspace_max_scroll = metrics.max_offset_from_bottom;
    hits.workspace_scroll_metrics = Some(metrics);
    if !view.mobile && frame.cache.rows.len() == 1 {
        if let SessionRowKind::Empty(title) = &frame.cache.rows[0].kind {
            let filtered = frame
                .endpoints
                .iter()
                .find(|endpoint| &endpoint.endpoint_id == frame.active_endpoint_id)
                .and_then(|endpoint| endpoint.snapshot.as_deref())
                .is_some_and(|snapshot| snapshot.agent_view_label.is_some());
            if !filtered {
                crate::ui::kit::empty_state::render_empty_state(
                    buffer,
                    body,
                    &crate::ui::kit::empty_state::EmptyState {
                        title,
                        ..Default::default()
                    },
                    palette,
                );
                return;
            }
        }
    }
    let show_scrollbar = metrics.max_offset_from_bottom > 0 && body.width > 1;
    let content_width = body.width.saturating_sub(u16::from(show_scrollbar));
    let mut y = body.y;
    let gaps = if view.mobile {
        &frame.cache.mobile_gaps
    } else {
        &frame.cache.gaps
    };
    for (index, row) in frame
        .cache
        .displayed_rows(view.mobile)
        .enumerate()
        .skip(view.start)
    {
        let height = row.height_for(view.mobile).min(body.height);
        if y.saturating_add(height) > body.bottom() {
            break;
        }
        let rect = Rect::new(body.x, y, content_width, height);
        paint_row(buffer, rect, row, frame, hits);
        if row.stale {
            buffer.set_style(
                rect,
                Style::default()
                    .fg(palette.overlay0)
                    .add_modifier(Modifier::DIM),
            );
        }
        y = y
            .saturating_add(height)
            .saturating_add(gaps.get(index).copied().unwrap_or(0));
    }
    if show_scrollbar {
        let track = Rect::new(body.right().saturating_sub(1), body.y, 1, body.height);
        hits.workspace_scrollbar = track;
        let thumb_hovered = matches!(frame.hover, Some(ChromeHover::WorkspaceScrollbarThumb));
        super::super::scroll::render_list_scrollbar(buffer, track, metrics, palette, thumb_hovered);
    }
    let top = if view.grouping.is_empty() {
        body.y
    } else {
        view.grouping.y
    };
    if let Some(row) = view
        .drop_row
        .filter(|row| *row >= top && *row < body.bottom())
    {
        for x in body.x..body.x.saturating_add(content_width) {
            buffer[(x, row)]
                .set_symbol("─")
                .set_style(Style::default().fg(palette.accent));
        }
    }
}

fn paint_toggle(
    buffer: &mut Buffer,
    x: u16,
    y: u16,
    collapsed: bool,
    hovered: bool,
    frame: &SessionPaint<'_>,
) {
    let palette = &frame.config.palette;
    paint_spans(
        buffer,
        x,
        y,
        x.saturating_add(1),
        &[Span::new(
            if collapsed { "▸" } else { "▾" },
            Style::default().fg(if hovered {
                palette.text
            } else {
                palette.overlay1
            }),
        )],
    );
}

fn paint_right(buffer: &mut Buffer, rect: Rect, y: u16, spans: &[Span<'_>]) -> u16 {
    let right = rect.right().saturating_sub(INSET);
    let width = super::spans_width(spans);
    if width == 0 || width.saturating_add(INSET) > rect.width {
        return right;
    }
    let x = right.saturating_sub(width);
    paint_spans(buffer, x, y, right, spans);
    x
}

fn rollup_spans<'a>(
    rollup: Option<Rollup>,
    frame: &SessionPaint<'_>,
    stale: bool,
) -> Vec<Span<'a>> {
    let Some(rollup) = rollup else {
        return Vec::new();
    };
    let palette = &frame.config.palette;
    let glyph_style = if stale {
        Style::default().fg(palette.overlay0)
    } else {
        crate::ui::design::status_style(rollup.bucket, palette, &frame.config.components)
    };
    let fixed = StatusGlyphs::fixed(frame.config.status_indicators);
    let mark = rollup.bucket == AgentBucket::NeedsInput
        && fixed.bucket(AgentBucket::NeedsInput) == fixed.bucket(AgentBucket::Working);
    let mut spans = Vec::with_capacity(2 + usize::from(mark));
    spans.push(Span::new(
        frame.glyphs_for(stale).bucket(rollup.bucket),
        glyph_style,
    ));
    if mark {
        spans.push(Span::new("!", glyph_style.add_modifier(Modifier::BOLD)));
    }
    spans.push(Span::new(
        if rollup.count == 0 {
            String::new()
        } else {
            format!(" {}", rollup.count)
        },
        crate::ui::design::meta(palette),
    ));
    spans
}

fn paint_row(
    buffer: &mut Buffer,
    rect: Rect,
    row: &SessionRow,
    frame: &SessionPaint<'_>,
    hits: &mut ShellHitMap,
) {
    let palette = &frame.config.palette;
    let x0 = rect.x.saturating_add(INSET);
    match &row.kind {
        SessionRowKind::MachineGroup { label } => {
            let budget = rect.right().saturating_sub(INSET).saturating_sub(x0);
            paint_spans(
                buffer,
                x0,
                rect.y,
                rect.right().saturating_sub(INSET),
                &[Span::new(
                    truncate_end(label, usize::from(budget)),
                    crate::ui::design::section(palette),
                )],
            );
        }
        SessionRowKind::Machine {
            label,
            status,
            collapsed,
            rollup,
            tag,
        } => {
            let hovered =
                matches!(frame.hover, Some(ChromeHover::MachineRow(id)) if id == &row.endpoint_id);
            if &row.endpoint_id == frame.active_endpoint_id && *collapsed {
                buffer.set_style(rect, Style::default().bg(palette.active_row_bg));
            } else {
                hover_bg(frame, rect, buffer, hovered);
            }
            if *status != ClientEndpointStatus::Online && !row.endpoint_id.is_local() {
                buffer.set_style(rect, Style::default().add_modifier(Modifier::DIM));
            }
            if let Some(tag) = tag {
                let color = match tag {
                    MachineTag::Color(color) => *color,
                    MachineTag::Hashed => {
                        super::super::machines_overlay::machine_hash_color(label, palette)
                    }
                };
                paint_spans(
                    buffer,
                    rect.x,
                    rect.y,
                    x0,
                    &[Span::new(
                        "▪",
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    )],
                );
            }
            paint_toggle(buffer, x0, rect.y, *collapsed, hovered, frame);
            let online = *status == ClientEndpointStatus::Online;
            let mut right_spans = rollup_spans(*rollup, frame, !online);
            if !row.endpoint_id.is_local() {
                let (glyph, state, color) = super::super::endpoints::endpoint_status_presentation(
                    *status,
                    palette,
                    frame.spinner,
                );
                if !right_spans.is_empty() {
                    right_spans.push(Span::new("  ", Style::default()));
                }
                right_spans.push(Span::new(glyph, Style::default().fg(color)));
                if !online {
                    right_spans.push(Span::new(
                        format!(" {state}"),
                        crate::ui::design::meta(palette),
                    ));
                }
            }
            let right = paint_right(buffer, rect, rect.y, &right_spans);
            let label_x = x0.saturating_add(2);
            let budget = right.saturating_sub(1).saturating_sub(label_x);
            let label_style = if *status == ClientEndpointStatus::Disabled {
                Style::default().fg(palette.overlay0)
            } else {
                Style::default()
                    .fg(palette.subtext0)
                    .add_modifier(Modifier::BOLD)
            };
            paint_spans(
                buffer,
                label_x,
                rect.y,
                right.saturating_sub(1),
                &[Span::new(
                    truncate_end(label, usize::from(budget)),
                    label_style,
                )],
            );
            hits.machines.push(MachineHit {
                rect,
                collapse_toggle: Rect::new(x0, rect.y, 2, 1),
                endpoint_id: row.endpoint_id.clone(),
            });
        }
        SessionRowKind::Workspace {
            workspace_id,
            label,
            meta,
            mobile_meta,
            extras,
            focused,
            indent,
            indented,
            last_child,
            toggle,
            rollup,
            holds_focus,
        } => {
            let selected = frame
                .view
                .selected
                .as_ref()
                .is_some_and(|target| target.matches(&row.endpoint_id, workspace_id));
            let dragged = frame.view.dragged.as_deref() == Some(workspace_id.as_str())
                && &row.endpoint_id == frame.active_endpoint_id;
            let hovered = matches!(frame.hover, Some(ChromeHover::WorkspaceRow { endpoint_id, workspace_id: hovered })
                if endpoint_id == &row.endpoint_id && hovered == workspace_id);
            let selection_bg = if frame.view.mobile {
                palette.surface_selection_bg()
            } else {
                palette.selection_row_bg()
            };
            if selected {
                buffer.set_style(rect, Style::default().bg(selection_bg));
            } else if dragged {
                buffer.set_style(rect, Style::default().bg(palette.surface1));
            } else if *holds_focus || *focused {
                let background =
                    if frame.view.selected.is_some() && selection_bg == palette.active_row_bg {
                        palette.sidebar_bg
                    } else {
                        palette.active_row_bg
                    };
                buffer.set_style(rect, Style::default().bg(background));
            } else {
                hover_bg(frame, rect, buffer, hovered);
            }
            if *indented {
                paint_spans(
                    buffer,
                    x0,
                    rect.y,
                    x0.saturating_add(2),
                    &[Span::new(
                        if *last_child { "└─" } else { "├─" },
                        crate::ui::design::meta(palette),
                    )],
                );
            }
            let x = x0.saturating_add(*indent);
            if let Some((key, collapsed)) = toggle {
                let hit = SessionHit::Toggle(row.endpoint_id.clone(), key.clone());
                paint_toggle(buffer, x, rect.y, *collapsed, frame.hovered(&hit), frame);
                hits.sessions.push((Rect::new(x, rect.y, 1, 1), hit));
            } else if *indented {
                paint_spans(
                    buffer,
                    x,
                    rect.y,
                    x.saturating_add(1),
                    &[Span::new("·", Style::default().fg(palette.overlay0))],
                );
            }
            let right = paint_right(
                buffer,
                rect,
                rect.y,
                &rollup_spans(*rollup, frame, row.stale),
            );
            let label_style = if *focused || dragged {
                crate::ui::design::title(palette)
            } else {
                Style::default().fg(palette.subtext0)
            };
            let text_x = x.saturating_add(2);
            let budget = right.saturating_sub(1).saturating_sub(text_x);
            let bucket = rollup.map_or(AgentBucket::Idle, |rollup| rollup.bucket);
            let extra_spans = crate::ui::resolved_token_spans(
                extras,
                ("", Style::default()),
                crate::ui::design::status_style(bucket, palette, &frame.config.components),
                label_style,
                crate::ui::design::meta(palette),
                crate::ui::design::meta(palette),
                palette,
                usize::from(budget),
            );
            let extra_width = extra_spans
                .iter()
                .map(|span| display_width_u16(span.content.as_ref()))
                .fold(0u16, u16::saturating_add);
            let label_budget =
                budget.saturating_sub(if extra_width > 0 { extra_width + 1 } else { 0 });
            let end = paint_spans(
                buffer,
                text_x,
                rect.y,
                right.saturating_sub(1),
                &[Span::new(
                    truncate_end(label, usize::from(label_budget)),
                    label_style,
                )],
            );
            let mut extra_x = end.saturating_add(1);
            for span in extra_spans {
                let (next, _) = buffer.set_stringn(
                    extra_x,
                    rect.y,
                    span.content.as_ref(),
                    usize::from(right.saturating_sub(1).saturating_sub(extra_x)),
                    span.style,
                );
                extra_x = next;
            }
            if let Some(meta) = (if frame.view.mobile {
                Some(mobile_meta.as_str())
            } else {
                meta.as_deref()
            })
            .filter(|_| rect.height > 1)
            {
                let y = rect.y + 1;
                let budget = rect.right().saturating_sub(INSET).saturating_sub(text_x);
                paint_spans(
                    buffer,
                    text_x,
                    y,
                    rect.right().saturating_sub(INSET),
                    &[Span::new(
                        truncate_end(meta, usize::from(budget)),
                        crate::ui::design::meta(palette),
                    )],
                );
            }
            hits.workspaces.push(WorkspaceHit {
                rect,
                endpoint_id: row.endpoint_id.clone(),
                workspace_id: workspace_id.clone(),
                indented: *indented,
                group_toggle: toggle
                    .as_ref()
                    .filter(|(key, _)| !key.starts_with("agent-panel:"))
                    .map(|(key, _)| (Rect::new(x, rect.y, 1, 1), key.clone())),
            });
        }
        SessionRowKind::Tab {
            tab_id,
            label,
            key,
            collapsed,
            rollup,
            indent,
            holds_focus,
        } => {
            let toggle = SessionHit::Toggle(row.endpoint_id.clone(), key.clone());
            let target = SessionHit::Tab(row.endpoint_id.clone(), tab_id.clone());
            if *holds_focus {
                buffer.set_style(rect, Style::default().bg(palette.active_row_bg));
            } else {
                hover_bg(
                    frame,
                    rect,
                    buffer,
                    frame.hovered(&toggle) || frame.hovered(&target),
                );
            }
            let x = x0.saturating_add(*indent);
            paint_toggle(buffer, x, rect.y, *collapsed, frame.hovered(&toggle), frame);
            let right = paint_right(
                buffer,
                rect,
                rect.y,
                &rollup_spans(*rollup, frame, row.stale),
            );
            let text_x = x.saturating_add(2);
            paint_spans(
                buffer,
                text_x,
                rect.y,
                right.saturating_sub(1),
                &[Span::new(
                    truncate_end(
                        label,
                        usize::from(right.saturating_sub(1).saturating_sub(text_x)),
                    ),
                    crate::ui::design::body(palette),
                )],
            );
            hits.sessions.push((Rect::new(x, rect.y, 1, 1), toggle));
            hits.sessions.push((
                Rect::new(text_x, rect.y, rect.right().saturating_sub(text_x), 1),
                target,
            ));
        }
        SessionRowKind::Agent(agent) => paint_agent_row(buffer, rect, row, agent, frame, hits),
        SessionRowKind::Terminal {
            pane_id,
            title,
            focused,
            indent,
        } => {
            let hit = SessionHit::Terminal(row.endpoint_id.clone(), pane_id.clone());
            if *focused {
                buffer.set_style(rect, Style::default().bg(palette.active_row_bg));
            } else {
                hover_bg(frame, rect, buffer, frame.hovered(&hit));
            }
            let x = x0.saturating_add(2).saturating_add(*indent);
            let budget = rect
                .right()
                .saturating_sub(INSET)
                .saturating_sub(x.saturating_add(2));
            paint_spans(
                buffer,
                x,
                rect.y,
                rect.right().saturating_sub(INSET),
                &[
                    Span::new("· ", Style::default().fg(palette.overlay0)),
                    Span::new(
                        truncate_end(title, usize::from(budget)),
                        if *focused {
                            Style::default().fg(palette.text)
                        } else {
                            crate::ui::design::meta(palette)
                        },
                    ),
                ],
            );
            hits.sessions.push((rect, hit));
        }
        SessionRowKind::Activity(tree_row, indent) => {
            let x = x0.saturating_add(2).saturating_add(*indent);
            let child = Rect::new(x, rect.y, rect.right().saturating_sub(x), 1);
            render_session_activity_row(
                buffer,
                child,
                tree_row,
                frame.config,
                frame.glyphs_for(row.stale),
                frame.hover,
                hits,
            );
        }
        SessionRowKind::ExternalGroup {
            key,
            label,
            collapsed,
            count,
        } => {
            let hit = SessionHit::Toggle(row.endpoint_id.clone(), key.clone());
            let hovered = frame.hovered(&hit);
            hover_bg(frame, rect, buffer, hovered);
            paint_toggle(buffer, x0, rect.y, *collapsed, hovered, frame);
            let right = paint_right(
                buffer,
                rect,
                rect.y,
                &[Span::new(count.as_str(), crate::ui::design::meta(palette))],
            );
            paint_spans(
                buffer,
                x0.saturating_add(2),
                rect.y,
                right.saturating_sub(1),
                &[Span::new(
                    label.as_str(),
                    Style::default().fg(palette.subtext0),
                )],
            );
            hits.sessions.push((rect, hit));
        }
        SessionRowKind::External {
            external_id,
            label,
            agent_name,
            readable,
            bucket,
            age,
            activity,
        } => {
            let hovered = matches!(frame.hover, Some(ChromeHover::ExternalAgentRow(endpoint_id, id)) if endpoint_id == &row.endpoint_id && id == external_id);
            hover_bg(frame, rect, buffer, hovered);
            let x = x0.saturating_add(2);
            let mut right = paint_right(
                buffer,
                rect,
                rect.y,
                &[Span::new(age.as_str(), crate::ui::design::meta(palette))],
            );
            if !age.is_empty() {
                right = right.saturating_sub(1);
            }
            let mut activity_visible = false;
            if let Some((key, _, chip)) = activity {
                let width = display_width_u16(chip);
                if right.saturating_sub(width + 1) >= x.saturating_add(2 + display_width_u16(label))
                {
                    activity_visible = true;
                    let hit = SessionHit::Toggle(row.endpoint_id.clone(), key.clone());
                    let chip_rect = Rect::new(right.saturating_sub(width), rect.y, width, 1);
                    paint_spans(
                        buffer,
                        chip_rect.x,
                        rect.y,
                        chip_rect.right(),
                        &[Span::new(chip.as_str(), crate::ui::design::meta(palette))],
                    );
                    hits.sessions.push((chip_rect, hit));
                    right = chip_rect.x.saturating_sub(1);
                }
            }
            if !activity_visible && !frame.view.mobile {
                if let Some((key, expanded, _)) = activity {
                    let hit = SessionHit::Toggle(row.endpoint_id.clone(), key.clone());
                    let toggle = Rect::new(x.saturating_sub(2), rect.y, 1, 1);
                    paint_toggle(
                        buffer,
                        toggle.x,
                        toggle.y,
                        !*expanded,
                        frame.hovered(&hit),
                        frame,
                    );
                    hits.sessions.push((toggle, hit));
                }
            }
            let budget = right.saturating_sub(x.saturating_add(2));
            let metadata = agent_name
                .as_deref()
                .into_iter()
                .chain((!*readable).then_some(crate::i18n::texts().agent_panel.external_unreadable))
                .collect::<Vec<_>>();
            let meta_width = metadata
                .iter()
                .map(|text| display_width_u16(text) + 1)
                .fold(0u16, u16::saturating_add);
            let label_budget = budget.saturating_sub(meta_width.min(budget.saturating_sub(6)));
            let mut spans = vec![
                Span::new(
                    frame.glyphs_for(row.stale).bucket(*bucket),
                    crate::ui::design::status_style(*bucket, palette, &frame.config.components),
                ),
                Span::new(" ", Style::default()),
                Span::new(
                    truncate_end(label, usize::from(label_budget)),
                    crate::ui::design::body(palette),
                ),
            ];
            let mut remaining = budget.saturating_sub(super::spans_width(&spans).saturating_sub(2));
            for text in metadata {
                if display_width_u16(text) < remaining {
                    spans.push(Span::new(" ", Style::default()));
                    spans.push(Span::new(text, crate::ui::design::meta(palette)));
                    remaining = remaining.saturating_sub(display_width_u16(text) + 1);
                }
            }
            paint_spans(buffer, x, rect.y, right, &spans);
            hits.external_agents
                .push((rect, row.endpoint_id.clone(), external_id.clone()));
        }
        SessionRowKind::Section { bucket, label } => {
            paint_spans(
                buffer,
                x0,
                rect.y,
                rect.right().saturating_sub(INSET),
                &[Span::new(
                    label.as_str(),
                    Style::default()
                        .fg(crate::ui::design::status_color(
                            *bucket,
                            palette,
                            &frame.config.components,
                        ))
                        .add_modifier(Modifier::BOLD),
                )],
            );
        }
        SessionRowKind::MobileAction {
            label,
            target,
            focused,
        } => {
            if *focused {
                buffer.set_style(rect, Style::default().bg(palette.active_row_bg));
            }
            paint_spans(
                buffer,
                x0,
                rect.y,
                rect.right().saturating_sub(INSET),
                &[Span::new(
                    label.as_str(),
                    if target.is_none() {
                        crate::ui::design::section(palette)
                    } else {
                        crate::ui::design::body(palette)
                    },
                )],
            );
            if let Some(target) = target {
                hits.mobile_targets.push((rect, target.clone()));
            }
        }
        SessionRowKind::Empty(text) => {
            paint_spans(
                buffer,
                x0,
                rect.y,
                rect.right().saturating_sub(INSET),
                &[Span::new(*text, crate::ui::design::meta(palette))],
            );
        }
    }
    if frame.view.mobile {
        let target = match &row.kind {
            SessionRowKind::Machine { .. } => {
                Some(ClientMobileTarget::Machine(row.endpoint_id.clone()))
            }
            SessionRowKind::Workspace { workspace_id, .. } => Some(ClientMobileTarget::Workspace {
                endpoint_id: row.endpoint_id.clone(),
                workspace_id: workspace_id.clone(),
            }),
            SessionRowKind::Agent(agent) => Some(ClientMobileTarget::Agent {
                endpoint_id: row.endpoint_id.clone(),
                pane_id: agent.pane_id.clone(),
            }),
            SessionRowKind::Terminal { pane_id, .. } => Some(ClientMobileTarget::Agent {
                endpoint_id: row.endpoint_id.clone(),
                pane_id: pane_id.clone(),
            }),
            _ => None,
        };
        if let Some(target) = target {
            let x = match &row.kind {
                SessionRowKind::Workspace { indent, .. } => {
                    rect.x.saturating_add(INSET + 2 + *indent)
                }
                SessionRowKind::Machine { .. } => rect.x.saturating_add(INSET + 2),
                _ => rect.x,
            }
            .min(rect.right());
            hits.mobile_targets.push((
                Rect::new(x, rect.y, rect.right().saturating_sub(x), rect.height),
                target,
            ));
        }
    }
}

fn paint_agent_row(
    buffer: &mut Buffer,
    rect: Rect,
    row: &SessionRow,
    agent: &SessionAgent,
    frame: &SessionPaint<'_>,
    hits: &mut ShellHitMap,
) {
    let palette = &frame.config.palette;
    let hovered = matches!(frame.hover, Some(ChromeHover::EndpointAgentRow(endpoint_id, pane_id))
        if endpoint_id == &row.endpoint_id && pane_id == &agent.pane_id);
    if agent.focused {
        buffer.set_style(rect, Style::default().bg(palette.active_row_bg));
    } else {
        hover_bg(frame, rect, buffer, hovered);
    }
    let x = rect
        .x
        .saturating_add(INSET)
        .saturating_add(if agent.location.is_some() { 0 } else { 2 })
        .saturating_add(agent.indent);
    let mut right = rect.right().saturating_sub(INSET);
    let name_end = x
        .saturating_add(2)
        .saturating_add(display_width_u16(&agent.name).min(right.saturating_sub(x + 2)));
    let mut activity_visible = false;
    if !agent.age.is_empty() {
        let width = display_width_u16(&agent.age);
        if right.saturating_sub(width + 1) >= name_end {
            right = right.saturating_sub(width);
            paint_spans(
                buffer,
                right,
                rect.y,
                right.saturating_add(width),
                &[Span::new(
                    agent.age.as_str(),
                    crate::ui::design::meta(palette),
                )],
            );
            right = right.saturating_sub(1);
        }
    }
    if let Some((key, _, chip)) = &agent.activity {
        let width = display_width_u16(chip);
        if right.saturating_sub(width + 1) >= name_end {
            activity_visible = true;
            let hit = SessionHit::Toggle(row.endpoint_id.clone(), key.clone());
            let chip_rect = Rect::new(right.saturating_sub(width), rect.y, width, 1);
            let style = if frame.hovered(&hit) {
                Style::default().fg(palette.text).bg(palette.hover_row_bg())
            } else {
                Style::default().fg(if agent.activity_running {
                    frame.config.components.status_running
                } else {
                    palette.overlay0
                })
            };
            paint_spans(
                buffer,
                chip_rect.x,
                rect.y,
                chip_rect.right(),
                &[Span::new(chip.as_str(), style)],
            );
            hits.sessions.push((chip_rect, hit));
            right = chip_rect.x.saturating_sub(1);
        }
    }
    if !activity_visible && !frame.view.mobile {
        if let Some((key, expanded, _)) = &agent.activity {
            let hit = SessionHit::Toggle(row.endpoint_id.clone(), key.clone());
            let toggle = Rect::new(x.saturating_sub(2), rect.y, 1, 1);
            paint_toggle(
                buffer,
                toggle.x,
                toggle.y,
                !*expanded,
                frame.hovered(&hit),
                frame,
            );
            hits.sessions.push((toggle, hit));
        }
    }
    let glyph_style = if row.stale {
        Style::default().fg(palette.overlay0)
    } else {
        crate::ui::design::status_style(agent.bucket, palette, &frame.config.components)
    };
    let fixed = StatusGlyphs::fixed(frame.config.status_indicators);
    let glyph = if agent.bucket == AgentBucket::NeedsInput
        && fixed.bucket(AgentBucket::NeedsInput) == fixed.bucket(AgentBucket::Working)
    {
        "!"
    } else {
        frame.glyphs_for(row.stale).bucket(agent.bucket)
    };
    let name_style = if agent.focused {
        crate::ui::design::title(palette)
    } else {
        crate::ui::design::body(palette)
    };
    if !agent.tokens.is_empty() {
        let first = if rect.height == 1 {
            agent
                .tokens
                .iter()
                .position(|tokens| {
                    tokens
                        .iter()
                        .any(|token| matches!(token.kind, crate::ui::ResolvedTokenKind::Agent(_)))
                })
                .unwrap_or(0)
        } else {
            0
        };
        for (index, tokens) in agent
            .tokens
            .iter()
            .skip(first)
            .take(usize::from(rect.height))
            .enumerate()
        {
            let y = rect.y.saturating_add(index as u16);
            let end = if index == 0 {
                right
            } else {
                rect.right().saturating_sub(INSET)
            };
            let mut cursor = x;
            if rect.height == 1
                && agent.tokens.len() > 1
                && !tokens
                    .iter()
                    .any(|token| matches!(token.kind, crate::ui::ResolvedTokenKind::StateIcon))
            {
                cursor = paint_spans(
                    buffer,
                    cursor,
                    y,
                    end,
                    &[
                        Span::new(glyph, glyph_style),
                        Span::new(" ", Style::default()),
                    ],
                );
            }
            let spans = crate::ui::resolved_token_spans(
                tokens,
                (glyph, glyph_style),
                glyph_style,
                name_style,
                crate::ui::design::meta(palette),
                crate::ui::design::meta(palette),
                palette,
                usize::from(end.saturating_sub(cursor)),
            );
            for span in spans {
                let (next, _) = buffer.set_stringn(
                    cursor,
                    y,
                    span.content.as_ref(),
                    usize::from(end.saturating_sub(cursor)),
                    span.style,
                );
                cursor = next;
            }
            if rect.height == 1
                && !tokens.iter().any(|token| {
                    matches!(
                        token.kind,
                        crate::ui::ResolvedTokenKind::Workspace(_)
                            | crate::ui::ResolvedTokenKind::Machine(_)
                    )
                })
            {
                if let Some(location) = &agent.location {
                    let remaining = end.saturating_sub(cursor + 3);
                    if remaining > 0 {
                        paint_spans(
                            buffer,
                            cursor,
                            y,
                            end,
                            &[
                                Span::new(" · ", crate::ui::design::meta(palette)),
                                Span::new(
                                    truncate_end(location, usize::from(remaining)),
                                    crate::ui::design::meta(palette),
                                ),
                            ],
                        );
                    }
                }
            }
        }
        hits.endpoint_agents
            .push((rect, row.endpoint_id.clone(), agent.pane_id.clone()));
        return;
    }
    let available = right.saturating_sub(x.saturating_add(2));
    let name_budget = available;
    let name = truncate_end(&agent.name, usize::from(name_budget));
    let mut spans = vec![
        Span::new(glyph, glyph_style),
        Span::new(" ", Style::default()),
        Span::new(name, name_style),
    ];
    if let Some(location) = agent.location.as_deref() {
        let used = super::spans_width(&spans);
        let room = right
            .saturating_sub(x)
            .saturating_sub(used)
            .saturating_sub(3);
        if room >= 3 {
            spans.push(Span::new(" · ", crate::ui::design::meta(palette)));
            spans.push(Span::new(
                truncate_end(location, usize::from(room.min(16))),
                crate::ui::design::meta(palette),
            ));
        }
    }
    let used = super::spans_width(&spans);
    let room = right
        .saturating_sub(x)
        .saturating_sub(used)
        .saturating_sub(2);
    if room >= SUMMARY_MIN && !agent.summary.is_empty() {
        spans.push(Span::new("  ", Style::default()));
        spans.push(Span::new(
            truncate_end(&agent.summary, usize::from(room)),
            crate::ui::design::muted(palette),
        ));
    }
    paint_spans(buffer, x, rect.y, right, &spans);
    hits.endpoint_agents
        .push((rect, row.endpoint_id.clone(), agent.pane_id.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{AgentAttentionKind, AgentStatus};
    use crate::protocol::{ClientShellAgentAttention, ClientShellAgentInsight};

    fn agent(status: AgentStatus, insight: ClientShellAgentInsight) -> ClientShellAgent {
        ClientShellAgent {
            insight,
            pane_id: "p1".into(),
            workspace_id: "ws".into(),
            tab_id: "t".into(),
            name: Some("claude".into()),
            display_agent: Some("claude".into()),
            agent: Some("claude".into()),
            title: Some("terminal title".into()),
            terminal_title: Some("terminal title".into()),
            terminal_title_stripped: Some("terminal title".into()),
            agent_status: status,
            state_change_seq: 0,
            state_labels: Vec::new(),
            tokens: Vec::new(),
            focused: false,
            launch_seq: 0,
            activity: Default::default(),
        }
    }
    fn full_insight() -> ClientShellAgentInsight {
        ClientShellAgentInsight {
            session_title: Some("修复焦点丢失".into()),
            current_tool: Some("Edit src/app/state.rs".into()),
            last_message: Some("All tests pass.\nSecond line".into()),
            question: Some("Allow edit?".into()),
            error: Some("exit 1".into()),
            working_since_ms: Some(1_000),
            state_changed_at_ms: Some(10_000),
            ..Default::default()
        }
    }
    #[test]
    fn summary_follows_the_bucket_priority() {
        let mut insight = full_insight();
        insight.attention = Some(ClientShellAgentAttention {
            kind: AgentAttentionKind::NeedsInput,
            detail: Some("Do you want to\n  make this edit?".into()),
            ..Default::default()
        });
        let blocked = agent(AgentStatus::Blocked, insight.clone());
        assert_eq!(
            session_summary(&blocked, AgentBucket::NeedsInput),
            "Do you want to"
        );
        insight.attention = None;
        let blocked = agent(AgentStatus::Blocked, insight.clone());
        assert_eq!(
            session_summary(&blocked, AgentBucket::NeedsInput),
            "Allow edit?"
        );
        let failed = agent(AgentStatus::Idle, insight.clone());
        assert_eq!(session_summary(&failed, AgentBucket::Failed), "exit 1");
        let working = agent(AgentStatus::Working, insight.clone());
        assert_eq!(
            session_summary(&working, AgentBucket::Working),
            "Edit src/app/state.rs"
        );
        insight.current_tool = None;
        let working = agent(AgentStatus::Working, insight.clone());
        assert_eq!(
            session_summary(&working, AgentBucket::Working),
            "修复焦点丢失"
        );
        let idle = agent(AgentStatus::Idle, insight.clone());
        assert_eq!(session_summary(&idle, AgentBucket::Idle), "修复焦点丢失");
        insight.session_title = None;
        let idle = agent(AgentStatus::Idle, insight);
        assert_eq!(session_summary(&idle, AgentBucket::Idle), "All tests pass.");
        let bare = agent(AgentStatus::Idle, ClientShellAgentInsight::default());
        assert_eq!(
            session_summary(&bare, AgentBucket::Idle),
            "terminal title",
            "旧 server 没有洞察时退回终端标题"
        );
    }
    #[test]
    fn age_uses_turn_start_while_working_and_state_change_otherwise() {
        let insight = full_insight();
        let working = agent(AgentStatus::Working, insight.clone());
        let now = 1_000 + 3 * 60_000 + 30_000;
        assert_eq!(session_age(&working, AgentBucket::Working, now), "3m");
        let idle = agent(AgentStatus::Idle, insight);
        assert_eq!(session_age(&idle, AgentBucket::Idle, now), "3m");
        assert_eq!(
            session_age(&idle, AgentBucket::Idle, 10_000 + 3_600_000),
            "1h"
        );
        assert_eq!(
            session_age(&idle, AgentBucket::Idle, 10_000 + 59_000),
            "now",
            "不足一分钟不按秒变化"
        );
        let bare = agent(AgentStatus::Idle, ClientShellAgentInsight::default());
        assert_eq!(session_age(&bare, AgentBucket::Idle, now), "");
    }
    #[test]
    fn rollup_counts_the_top_bucket_and_hides_idle() {
        let rollup = Rollup::of(
            [
                AgentBucket::Working,
                AgentBucket::Idle,
                AgentBucket::Working,
                AgentBucket::Review,
            ]
            .into_iter(),
        );
        assert_eq!(
            rollup,
            Some(Rollup {
                bucket: AgentBucket::Working,
                count: 2
            })
        );
        assert_eq!(
            Rollup::of([AgentBucket::Idle, AgentBucket::None].into_iter()),
            None
        );
        assert_eq!(Rollup::of(std::iter::empty()), None);
    }
    #[test]
    fn grouping_round_trips_through_serde_and_toggles() {
        assert_eq!(
            SessionGrouping::Workspace.toggled(),
            SessionGrouping::Status
        );
        assert_eq!(
            SessionGrouping::Status.toggled(),
            SessionGrouping::Workspace
        );
        assert_eq!(
            serde_json::to_string(&SessionGrouping::Status).expect("encode"),
            "\"status\""
        );
        assert_eq!(
            serde_json::from_str::<SessionGrouping>("\"workspace\"").expect("decode"),
            SessionGrouping::Workspace
        );
    }
    #[test]
    fn single_line_collapses_whitespace() {
        assert_eq!(single_line("\n  a   b \n c"), "a b");
        assert_eq!(single_line("   "), "");
    }
}
