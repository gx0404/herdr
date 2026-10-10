//! 六家官方 harness 的声明式注册表（借鉴 Paseo 的 `AGENT_PROVIDER_DEFINITIONS` /
//! `AGENT_HOOK_PROVIDERS`）：来源标签、钩子权威、配置目录、会话恢复命令与能力位都在
//! 这一张表里。散落在检测、恢复、活动与声音配置里的名单改为查表，新增或调整一家只改
//! 这里。

use std::path::{Path, PathBuf};

/// 钩子上报对状态的权威程度（状态仲裁见 `terminal::TerminalState`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookAuthority {
    /// 没有钩子（不在 pane 里运行的外部来源）。
    None,
    /// 钩子只报会话身份与详情，状态由屏幕检测决定（Claude Code）。
    SessionOnly,
    /// 钩子报 working / idle，屏幕上新鲜的阻塞仍可覆盖（Codex）。
    Partial,
    /// 钩子是完整生命周期权威，屏幕不再读取（Kimi / OpenCode / Pi）。
    FullLifecycle,
}

/// 活动来源读数据的方式，决定调度发现走哪条后台车道：慢的子进程不得拖住读文件的
/// 来源（`server::agent_activity` 的发现车道）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryLane {
    /// 读会话文件或内存里的快照，单次有界且快。
    Files,
    /// 起子进程查询（`opencode db`、`sqlite3`），单次可达数秒。
    Subprocess,
}

/// 会话恢复时会话引用的形状。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeRef {
    /// 只接受会话 id。
    Id,
    /// 会话文件路径或 id 都可以（Pi）。
    PathOrId,
}

/// 一家官方 harness 的静态描述。
#[derive(Debug)]
pub struct HarnessSpec {
    /// 规范化 agent 名（与检测、活动适配器一致）。
    pub id: &'static str,
    /// 面向用户的名称。
    #[allow(dead_code)] // 客户端 Inspector / 总览展示 harness 名称时接入（阶段 5）
    pub display_name: &'static str,
    /// 官方集成上报时用的来源标签。
    pub source: &'static str,
    pub hook_authority: HookAuthority,
    /// 在 pane 里运行（参与检测、会话恢复与状态上报）；外部来源为 `false`。
    pub pane_harness: bool,
    /// 钩子随会话上报转录路径（活动适配器据此直接定位会话文件）。
    pub reports_transcript: bool,
    /// 覆盖配置目录的环境变量。
    pub config_env: Option<&'static str>,
    /// `home` 下的默认配置目录。
    pub config_default: &'static [&'static str],
    /// 恢复命令：程序名与会话参数（`None` 表示不支持恢复）。
    pub resume: Option<(&'static str, &'static [&'static str], ResumeRef)>,
    /// 活动来源的发现车道。
    pub discovery: DiscoveryLane,
}

/// 六家官方 harness。顺序即展示顺序。
pub const HARNESSES: &[HarnessSpec] = &[
    HarnessSpec {
        id: "claude",
        display_name: "Claude Code",
        source: "herdr:claude",
        hook_authority: HookAuthority::SessionOnly,
        pane_harness: true,
        reports_transcript: true,
        config_env: Some("CLAUDE_CONFIG_DIR"),
        config_default: &[".claude"],
        resume: Some(("claude", &["--resume"], ResumeRef::Id)),
        discovery: DiscoveryLane::Files,
    },
    HarnessSpec {
        id: "codex",
        display_name: "Codex",
        source: "herdr:codex",
        hook_authority: HookAuthority::Partial,
        pane_harness: true,
        reports_transcript: true,
        config_env: Some("CODEX_HOME"),
        config_default: &[".codex"],
        resume: Some(("codex", &["resume"], ResumeRef::Id)),
        discovery: DiscoveryLane::Files,
    },
    HarnessSpec {
        id: "kimi",
        display_name: "Kimi Code",
        source: "herdr:kimi",
        hook_authority: HookAuthority::FullLifecycle,
        pane_harness: true,
        reports_transcript: true,
        config_env: Some("KIMI_CODE_HOME"),
        config_default: &[".kimi-code"],
        resume: Some(("kimi", &["--session"], ResumeRef::Id)),
        discovery: DiscoveryLane::Files,
    },
    HarnessSpec {
        id: "opencode",
        display_name: "OpenCode",
        source: "herdr:opencode",
        hook_authority: HookAuthority::FullLifecycle,
        pane_harness: true,
        reports_transcript: false,
        config_env: None,
        config_default: &[".config", "opencode"],
        resume: Some(("opencode", &["--session"], ResumeRef::Id)),
        discovery: DiscoveryLane::Subprocess,
    },
    HarnessSpec {
        id: "pi",
        display_name: "Pi",
        source: "herdr:pi",
        hook_authority: HookAuthority::FullLifecycle,
        pane_harness: true,
        reports_transcript: false,
        config_env: Some("PI_CODING_AGENT_DIR"),
        config_default: &[".pi", "agent"],
        resume: Some(("pi", &["--session"], ResumeRef::PathOrId)),
        discovery: DiscoveryLane::Files,
    },
    HarnessSpec {
        id: "zcode",
        display_name: "ZCode",
        source: "herdr:zcode",
        hook_authority: HookAuthority::None,
        pane_harness: false,
        reports_transcript: false,
        config_env: None,
        config_default: &[".zcode", "cli"],
        resume: None,
        discovery: DiscoveryLane::Subprocess,
    },
];

/// 按规范化 agent 名查找。
pub fn by_id(id: &str) -> Option<&'static HarnessSpec> {
    HARNESSES.iter().find(|spec| spec.id == id)
}

/// 按「来源标签 + agent 名」查找官方集成：两者必须同时对上（`herdr:claude` 报
/// `codex` 不算官方）。
pub fn official(source: &str, agent: &str) -> Option<&'static HarnessSpec> {
    by_id(agent).filter(|spec| spec.source == source)
}

/// 官方、且在 pane 里运行的集成来源（参与会话恢复）。
pub fn is_official_pane_source(source: &str, agent: &str) -> bool {
    official(source, agent).is_some_and(|spec| spec.pane_harness)
}

/// 官方集成的钩子是否为完整生命周期权威。
pub fn has_full_lifecycle_authority(source: &str, agent: &str) -> bool {
    official(source, agent).is_some_and(|spec| spec.hook_authority == HookAuthority::FullLifecycle)
}

/// 官方集成的状态上报是否只当会话身份处理（屏幕检测保留状态权威）。
pub fn reserves_native_state(source: &str, agent: &str) -> bool {
    official(source, agent).is_some_and(|spec| spec.hook_authority == HookAuthority::SessionOnly)
}

/// 官方集成是否随会话上报转录路径。
pub fn reports_transcript(source: &str, agent: &str) -> bool {
    official(source, agent).is_some_and(|spec| spec.reports_transcript)
}

/// 活动来源的发现车道；不在表里的来源按读文件处理。
pub fn discovery_lane(id: &str) -> DiscoveryLane {
    by_id(id).map_or(DiscoveryLane::Files, |spec| spec.discovery)
}

/// 该 harness 的配置目录：环境变量覆盖优先（支持 `~` 前缀），否则 `home` 下的默认目录。
pub fn config_dir(id: &str, home: &Path) -> Option<PathBuf> {
    let spec = by_id(id)?;
    if let Some(value) = spec
        .config_env
        .and_then(std::env::var_os)
        .filter(|value| !value.is_empty())
    {
        return Some(expand_tilde(PathBuf::from(value), home));
    }
    let mut dir = home.to_path_buf();
    dir.extend(spec.config_default);
    Some(dir)
}

/// 恢复命令的 argv；会话引用形状不被接受时为 `None`。
pub fn resume_argv(id: &str, session_is_path: bool, session: &str) -> Option<Vec<String>> {
    let (program, flags, accepts) = by_id(id)?.resume?;
    if session_is_path && accepts == ResumeRef::Id {
        return None;
    }
    let mut argv = Vec::with_capacity(flags.len() + 2);
    argv.push(program.to_owned());
    argv.extend(flags.iter().map(|flag| (*flag).to_owned()));
    argv.push(session.to_owned());
    Some(argv)
}

/// `~` / `~/rest` / `~\rest` 展开到 `home`；其余原样。
pub fn expand_tilde(path: PathBuf, home: &Path) -> PathBuf {
    let Some(raw) = path.to_str() else {
        return path;
    };
    if raw == "~" {
        return home.to_path_buf();
    }
    match raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        Some(rest) => home.join(rest),
        None => path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_requires_matching_source_and_agent() {
        assert!(official("herdr:claude", "claude").is_some());
        assert!(official("herdr:claude", "codex").is_none());
        assert!(official("custom", "claude").is_none());
        assert!(is_official_pane_source("herdr:pi", "pi"));
        assert!(!is_official_pane_source("herdr:zcode", "zcode"));
    }

    #[test]
    fn authority_table_matches_the_arbitration_rules() {
        assert!(reserves_native_state("herdr:claude", "claude"));
        assert!(!reserves_native_state("herdr:codex", "codex"));
        for agent in ["kimi", "opencode", "pi"] {
            assert!(
                has_full_lifecycle_authority(&format!("herdr:{agent}"), agent),
                "{agent}"
            );
        }
        assert!(!has_full_lifecycle_authority("herdr:codex", "codex"));
        assert!(reports_transcript("herdr:kimi", "kimi"));
        assert!(!reports_transcript("herdr:pi", "pi"));
    }

    #[test]
    fn subprocess_sources_get_their_own_discovery_lane() {
        for id in ["claude", "codex", "kimi", "pi", "unknown"] {
            assert_eq!(discovery_lane(id), DiscoveryLane::Files, "{id}");
        }
        for id in ["opencode", "zcode"] {
            assert_eq!(discovery_lane(id), DiscoveryLane::Subprocess, "{id}");
        }
    }

    #[test]
    fn resume_argv_follows_each_cli() {
        assert_eq!(
            resume_argv("claude", false, "abc").unwrap(),
            ["claude", "--resume", "abc"]
        );
        assert_eq!(
            resume_argv("codex", false, "t1").unwrap(),
            ["codex", "resume", "t1"]
        );
        assert_eq!(
            resume_argv("pi", true, "/s/a.jsonl").unwrap(),
            ["pi", "--session", "/s/a.jsonl"]
        );
        assert!(resume_argv("claude", true, "/x").is_none());
        assert!(resume_argv("zcode", false, "x").is_none());
    }

    #[test]
    fn config_dir_defaults_under_home() {
        let home = Path::new("/srv/home");
        assert_eq!(
            config_dir("opencode", home),
            Some(home.join(".config").join("opencode"))
        );
        assert_eq!(
            config_dir("zcode", home),
            Some(home.join(".zcode").join("cli"))
        );
        assert!(config_dir("unknown", home).is_none());
    }

    #[test]
    fn tilde_expansion() {
        let home = Path::new("/h");
        assert_eq!(expand_tilde("~".into(), home), PathBuf::from("/h"));
        assert_eq!(
            expand_tilde("~/x".into(), home),
            PathBuf::from("/h").join("x")
        );
        assert_eq!(expand_tilde("/abs".into(), home), PathBuf::from("/abs"));
    }
}
