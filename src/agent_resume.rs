use std::path::Path;

use serde::{Deserialize, Serialize};

const MAX_SESSION_ID_LEN: usize = 512;
const MAX_SESSION_PATH_LEN: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSessionRef {
    pub kind: AgentSessionRefKind,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentSessionRefKind {
    Id,
    Path,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentResumePlan {
    pub agent: String,
    pub argv: Vec<String>,
    pub dedupe_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedAgentSession {
    pub source: String,
    pub agent: String,
    pub session_ref: AgentSessionRef,
}

impl AgentSessionRef {
    pub fn id(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        valid_session_id(&value).then_some(Self {
            kind: AgentSessionRefKind::Id,
            value,
        })
    }

    pub fn path(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        valid_session_path(&value).then_some(Self {
            kind: AgentSessionRefKind::Path,
            value,
        })
    }
}

pub fn session_ref_from_report(
    source: &str,
    agent: &str,
    agent_session_id: Option<String>,
    _agent_session_path: Option<String>,
) -> Option<AgentSessionRef> {
    if !is_official_agent_source(source, agent) {
        return None;
    }

    if agent == "pi" {
        return _agent_session_path
            .and_then(AgentSessionRef::path)
            .or_else(|| agent_session_id.and_then(AgentSessionRef::id));
    }

    agent_session_id.and_then(AgentSessionRef::id)
}

/// 钩子随会话一起上报的转录文件路径（[`transcript_from_report`]）：只给活动树定位会话
/// 文件用，恢复仍只认 [`session_ref_from_report`] 给出的会话引用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportedTranscript {
    /// 同一次上报的会话 id：活动树只在 pane 的会话 id 仍是它时才用这条路径。
    pub session_id: String,
    /// 转录文件的绝对路径（[`AgentSessionRefKind::Path`]）。
    pub path: AgentSessionRef,
}

/// 钩子上报的转录路径。目前只收 claude 官方集成的：SessionStart 载荷的
/// `transcript_path` 与会话 id 一起上报，活动适配器由它推出会话目录。路径是 pane 里的
/// CLI 自己给的，`CLAUDE_CONFIG_DIR` 只在 pane 里设置时活动树也能找到会话文件。其余
/// agent 不收：codex 钩子拿到了转录路径但没有转发，kimi 的钩子载荷里没有路径，pi 的
/// 路径本身就是会话引用。
///
/// 来源名是上报方自报的，所以路径也要像 claude 的主转录：绝对路径、文件名恰是
/// `<会话 id>.jsonl`，不以分隔符结尾（目录写法）；指向目录的在适配器里再拒一次。
pub fn transcript_from_report(
    source: &str,
    agent: &str,
    agent_session_id: Option<&str>,
    agent_session_path: Option<&str>,
) -> Option<ReportedTranscript> {
    if (source, agent) != ("herdr:claude", "claude") {
        return None;
    }
    let session_id = agent_session_id.filter(|id| valid_session_id(id))?;
    let raw = agent_session_path?;
    let named = Path::new(raw).file_name().and_then(|name| name.to_str())
        == Some(format!("{session_id}.jsonl").as_str());
    if !named || raw.ends_with(['/', '\\']) {
        return None;
    }
    let path = AgentSessionRef::path(raw)?;
    Some(ReportedTranscript {
        session_id: session_id.to_owned(),
        path,
    })
}

pub fn persisted_session_from_launch_args(
    agent: crate::detect::Agent,
    args: &[String],
) -> Option<PersistedAgentSession> {
    let [command, session_id] = args else {
        return None;
    };
    if agent != crate::detect::Agent::Codex || command != "resume" || session_id.starts_with('-') {
        return None;
    }

    Some(PersistedAgentSession {
        source: "herdr:codex".into(),
        agent: "codex".into(),
        session_ref: AgentSessionRef::id(session_id.clone())?,
    })
}

pub fn normalize_session_start_source(value: Option<String>) -> Option<String> {
    match value.as_deref().map(str::trim) {
        Some(
            source @ ("startup" | "resume" | "clear" | "compact" | "branch" | "new" | "fork"
            | "select"),
        ) => Some(source.to_string()),
        _ => None,
    }
}

pub fn is_reserved_native_state_source(source: &str, agent: &str) -> bool {
    matches!(
        (source, agent),
        ("herdr:claude", "claude") | ("herdr:codex", "codex")
    )
}

pub fn session_ref_from_snapshot(
    source: &str,
    agent: &str,
    kind: AgentSessionRefKind,
    value: &str,
) -> Option<PersistedAgentSession> {
    if !is_official_agent_source(source, agent) {
        return None;
    }
    let session_ref = match (agent, kind) {
        ("pi", AgentSessionRefKind::Path) => AgentSessionRef::path(value)?,
        (_, AgentSessionRefKind::Id) => AgentSessionRef::id(value)?,
        _ => return None,
    };
    Some(PersistedAgentSession {
        source: source.to_string(),
        agent: agent.to_string(),
        session_ref,
    })
}

pub fn plan(source: &str, agent: &str, session_ref: &AgentSessionRef) -> Option<AgentResumePlan> {
    if !is_official_agent_source(source, agent) {
        return None;
    }

    let argv = match (source, agent, session_ref.kind) {
        ("herdr:claude", "claude", AgentSessionRefKind::Id) => {
            vec![
                "claude".into(),
                "--resume".into(),
                session_ref.value.clone(),
            ]
        }
        ("herdr:codex", "codex", AgentSessionRefKind::Id) => {
            vec!["codex".into(), "resume".into(), session_ref.value.clone()]
        }
        ("herdr:kimi", "kimi", AgentSessionRefKind::Id) => {
            vec!["kimi".into(), "--session".into(), session_ref.value.clone()]
        }
        ("herdr:pi", "pi", AgentSessionRefKind::Path | AgentSessionRefKind::Id) => {
            vec!["pi".into(), "--session".into(), session_ref.value.clone()]
        }
        ("herdr:opencode", "opencode", AgentSessionRefKind::Id) => {
            vec![
                "opencode".into(),
                "--session".into(),
                session_ref.value.clone(),
            ]
        }
        _ => return None,
    };

    Some(AgentResumePlan {
        agent: agent.to_string(),
        argv,
        dedupe_key: dedupe_key(source, agent, session_ref),
    })
}

pub fn dedupe_key(source: &str, agent: &str, session_ref: &AgentSessionRef) -> String {
    format!(
        "{source}\u{0}{agent}\u{0}{:?}\u{0}{}",
        session_ref.kind, session_ref.value
    )
}

/// 官方集成的保留来源：只有五家官方集成能以 `herdr:<agent>` 上报会话并参与恢复。
/// 已删除集成残留的 hook 即使仍在上报，也按非官方来源处理（不落会话、不生成恢复计划）。
pub(crate) fn is_official_agent_source(source: &str, agent: &str) -> bool {
    matches!(
        (source, agent),
        ("herdr:claude", "claude")
            | ("herdr:codex", "codex")
            | ("herdr:kimi", "kimi")
            | ("herdr:pi", "pi")
            | ("herdr:opencode", "opencode")
    )
}

fn valid_session_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_SESSION_ID_LEN && !value.chars().any(char::is_control)
}

fn valid_session_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SESSION_PATH_LEN
        && !value.chars().any(char::is_control)
        && Path::new(value).is_absolute()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn absolute_test_path(name: &str) -> String {
        std::env::current_dir()
            .unwrap()
            .join(name)
            .display()
            .to_string()
    }

    #[test]
    fn native_state_reservation_excludes_full_lifecycle_sources() {
        assert!(is_reserved_native_state_source("herdr:claude", "claude"));
        assert!(is_reserved_native_state_source("herdr:codex", "codex"));
        assert!(!is_reserved_native_state_source("herdr:kimi", "kimi"));
        assert!(!is_reserved_native_state_source("herdr:pi", "pi"));
        assert!(!is_reserved_native_state_source(
            "herdr:opencode",
            "opencode"
        ));
    }

    #[test]
    fn official_sources_are_exactly_the_five_integrations() {
        for agent in crate::detect::Agent::ALL {
            let label = crate::detect::agent_label(agent);
            assert!(is_official_agent_source(&format!("herdr:{label}"), label));
            // 来源与标签必须成对；自定义来源永远不是官方来源。
            assert!(!is_official_agent_source(&format!("custom:{label}"), label));
            assert!(!is_official_agent_source("herdr:other", label));
        }
        // 已删除集成残留的 hook 仍可能上报：按非官方来源处理，不落会话、不生成恢复计划。
        for label in crate::detect::RETIRED_AGENT_LABELS {
            let source = format!("herdr:{label}");
            assert!(!is_official_agent_source(&source, label), "{label}");
            assert!(
                session_ref_from_report(&source, label, Some("session".into()), None).is_none(),
                "{label}"
            );
            assert!(
                session_ref_from_snapshot(&source, label, AgentSessionRefKind::Id, "session")
                    .is_none(),
                "{label}"
            );
            assert!(
                plan(&source, label, &AgentSessionRef::id("session").unwrap()).is_none(),
                "{label}"
            );
        }
    }

    #[test]
    fn codex_noncanonical_resume_launch_has_no_explicit_session() {
        assert_eq!(
            persisted_session_from_launch_args(
                crate::detect::Agent::Codex,
                &["resume".into(), "codex-session".into()]
            )
            .unwrap()
            .session_ref
            .value,
            "codex-session"
        );
        assert!(persisted_session_from_launch_args(
            crate::detect::Agent::Codex,
            &["resume".into(), "--last".into()]
        )
        .is_none());
        assert!(persisted_session_from_launch_args(
            crate::detect::Agent::Codex,
            &["resume".into(), "not-a-session".into(), "--last".into()]
        )
        .is_none());
        assert!(persisted_session_from_launch_args(
            crate::detect::Agent::Codex,
            &[
                "--remote".into(),
                "ws://example.test".into(),
                "resume".into(),
                "remote-session".into(),
            ]
        )
        .is_none());
    }

    #[test]
    fn planner_allows_supported_agents() {
        let pi_session = absolute_test_path("pi-session.jsonl");
        assert_eq!(
            plan(
                "herdr:claude",
                "claude",
                &AgentSessionRef::id("claude-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["claude", "--resume", "claude-session"]
        );
        assert_eq!(
            plan(
                "herdr:codex",
                "codex",
                &AgentSessionRef::id("codex-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["codex", "resume", "codex-session"]
        );
        assert_eq!(
            plan(
                "herdr:kimi",
                "kimi",
                &AgentSessionRef::id("kimi-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["kimi", "--session", "kimi-session"]
        );
        assert_eq!(
            plan(
                "herdr:pi",
                "pi",
                &AgentSessionRef::path(&pi_session).unwrap()
            )
            .unwrap()
            .argv,
            vec!["pi", "--session", pi_session.as_str()]
        );
        assert_eq!(
            plan("herdr:pi", "pi", &AgentSessionRef::id("pi-id").unwrap())
                .unwrap()
                .argv,
            vec!["pi", "--session", "pi-id"]
        );
        assert_eq!(
            plan(
                "herdr:opencode",
                "opencode",
                &AgentSessionRef::id("opencode-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["opencode", "--session", "opencode-session"]
        );
    }

    #[test]
    fn planner_rejects_custom_and_unsupported_path_refs() {
        let claude_session = absolute_test_path("claude-session");
        assert!(plan(
            "custom:claude",
            "claude",
            &AgentSessionRef::id("session").unwrap()
        )
        .is_none());
        assert!(plan(
            "herdr:claude",
            "claude",
            &AgentSessionRef::path(&claude_session).unwrap()
        )
        .is_none());
    }

    #[test]
    fn report_ref_prefers_pi_paths_and_validates_values() {
        let pi_session = absolute_test_path("pi-session.jsonl");
        let claude_session = absolute_test_path("claude-session");
        let session_ref = session_ref_from_report(
            "herdr:pi",
            "pi",
            Some("pi-id".into()),
            Some(pi_session.clone()),
        )
        .unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Path);
        assert_eq!(session_ref.value, pi_session);

        assert!(session_ref_from_report("herdr:pi", "pi", Some("bad\nid".into()), None).is_none());
        assert!(
            session_ref_from_report("herdr:pi", "pi", None, Some("relative.jsonl".into()))
                .is_none()
        );
        assert!(session_ref_from_report("custom:pi", "pi", Some("pi-id".into()), None).is_none());

        // 路径不是绝对路径时退回 id。
        let session_ref = session_ref_from_report(
            "herdr:pi",
            "pi",
            Some("pi-id".into()),
            Some("relative.jsonl".into()),
        )
        .unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Id);
        assert_eq!(session_ref.value, "pi-id");

        assert!(
            session_ref_from_report("herdr:claude", "claude", None, Some(claude_session)).is_none()
        );

        for (source, agent) in [
            ("herdr:claude", "claude"),
            ("herdr:codex", "codex"),
            ("herdr:kimi", "kimi"),
            ("herdr:opencode", "opencode"),
        ] {
            let session_ref =
                session_ref_from_report(source, agent, Some("session-id".into()), None).unwrap();
            assert_eq!(session_ref.kind, AgentSessionRefKind::Id, "{agent}");
            assert_eq!(session_ref.value, "session-id", "{agent}");
        }
    }

    /// 交接 T8 G1b：claude 钩子随会话上报的转录路径与会话 id 成对保留，给活动树用；
    /// 只收 claude 官方来源、合法 id 与绝对路径，恢复用的会话引用不受影响。
    #[test]
    fn reported_transcripts_pair_claude_paths_with_their_session_id() {
        let transcript = absolute_test_path("session-id.jsonl");
        let reported = transcript_from_report(
            "herdr:claude",
            "claude",
            Some("session-id"),
            Some(&transcript),
        )
        .expect("claude 的转录路径");
        assert_eq!(reported.session_id, "session-id");
        assert_eq!(reported.path.kind, AgentSessionRefKind::Path);
        assert_eq!(reported.path.value, transcript);
        assert_eq!(
            session_ref_from_report(
                "herdr:claude",
                "claude",
                Some("session-id".into()),
                Some(transcript.clone())
            )
            .map(|session| session.kind),
            Some(AgentSessionRefKind::Id),
            "恢复仍按 id"
        );

        // 文件名必须恰是 `<会话 id>.jsonl`：别的文件、别的扩展名、以分隔符结尾的都不收。
        let other = absolute_test_path("other.jsonl");
        let json = absolute_test_path("session-id.json");
        let trailing = format!("{transcript}{}", std::path::MAIN_SEPARATOR);
        for (source, agent, id, path) in [
            (
                "herdr:claude",
                "claude",
                Some("session-id"),
                Some(other.as_str()),
            ),
            (
                "herdr:claude",
                "claude",
                Some("session-id"),
                Some(json.as_str()),
            ),
            (
                "herdr:claude",
                "claude",
                Some("session-id"),
                Some(trailing.as_str()),
            ),
            ("herdr:claude", "claude", None, Some(transcript.as_str())),
            (
                "herdr:claude",
                "claude",
                Some("bad\nid"),
                Some(transcript.as_str()),
            ),
            (
                "herdr:claude",
                "claude",
                Some("session-id"),
                Some("relative.jsonl"),
            ),
            ("herdr:claude", "claude", Some("session-id"), None),
            (
                "custom:claude",
                "claude",
                Some("session-id"),
                Some(transcript.as_str()),
            ),
            (
                "herdr:codex",
                "codex",
                Some("session-id"),
                Some(transcript.as_str()),
            ),
            (
                "herdr:pi",
                "pi",
                Some("session-id"),
                Some(transcript.as_str()),
            ),
        ] {
            assert!(
                transcript_from_report(source, agent, id, path).is_none(),
                "{source} {agent} {id:?} {path:?}"
            );
        }
    }

    #[test]
    fn normalize_session_start_source_allows_known_values() {
        assert_eq!(
            normalize_session_start_source(Some("startup".into())),
            Some("startup".into())
        );
        assert_eq!(
            normalize_session_start_source(Some("resume".into())),
            Some("resume".into())
        );
        assert_eq!(
            normalize_session_start_source(Some("clear".into())),
            Some("clear".into())
        );
        assert_eq!(
            normalize_session_start_source(Some("compact".into())),
            Some("compact".into())
        );
        assert_eq!(
            normalize_session_start_source(Some("branch".into())),
            Some("branch".into())
        );
        assert_eq!(
            normalize_session_start_source(Some("new".into())),
            Some("new".into())
        );
        assert_eq!(
            normalize_session_start_source(Some("fork".into())),
            Some("fork".into())
        );
        assert_eq!(
            normalize_session_start_source(Some("select".into())),
            Some("select".into())
        );
        assert_eq!(
            normalize_session_start_source(Some(" resume ".into())),
            Some("resume".into())
        );
        assert_eq!(normalize_session_start_source(Some("other".into())), None);
        assert_eq!(normalize_session_start_source(None), None);
    }

    #[test]
    fn ids_are_data_not_shell_text() {
        let id = "abc; rm -rf /";
        let codex_plan = plan("herdr:codex", "codex", &AgentSessionRef::id(id).unwrap()).unwrap();
        assert_eq!(codex_plan.argv, vec!["codex", "resume", id]);

        let claude_plan =
            plan("herdr:claude", "claude", &AgentSessionRef::id(id).unwrap()).unwrap();
        assert_eq!(claude_plan.argv, vec!["claude", "--resume", id]);
    }

    #[test]
    fn planner_rejects_path_refs_for_id_only_agents() {
        for agent in ["claude", "codex", "kimi", "opencode"] {
            let source = format!("herdr:{agent}");
            let session = absolute_test_path(&format!("{agent}-session"));
            assert!(
                plan(&source, agent, &AgentSessionRef::path(&session).unwrap()).is_none(),
                "{agent}"
            );
            assert!(
                session_ref_from_snapshot(&source, agent, AgentSessionRefKind::Path, &session)
                    .is_none(),
                "{agent}"
            );
            assert!(
                session_ref_from_snapshot(&source, agent, AgentSessionRefKind::Id, "session")
                    .is_some(),
                "{agent}"
            );
        }
    }
}
