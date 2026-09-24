//! 集成上报的来源校验。
//!
//! herdr 自带的集成资产（source 以 `herdr:` 开头）总是靠继承来的 `HERDR_PANE_ID`
//! 定位窗格。脱离窗格运行、却继承了窗格环境的进程（agent 的后台会话、daemon、
//! `nohup` / `systemd-run` 拉起的进程）也带着同一个编号，它们的上报会冒充那个窗格。
//! 这里在 API 连接线程上取对端 pid、把它的父链快照下来（连接存活期间对端一定在等
//! 应答，所有集成资产都等应答后才退出），事件循环再拿窗格根进程判定：
//!
//! - 对端是窗格根进程的后代（或就是它）→ 接受；
//! - 父链完整走到根也没有窗格根进程 → 静默丢弃（照常回成功，不改状态）；
//! - 拿不到对端 pid、父链读不全、窗格没有根进程 → 查不清，放行（fail open）。
//!
//! 进程查询只发生在 API 连接线程上，事件循环只做集合查找。

use crate::api::schema::Method;
use crate::platform::ProcessLineage;

/// herdr 自带集成资产的 source 前缀；只有这类上报走来源校验。
const BUNDLED_INTEGRATION_SOURCE_PREFIX: &str = "herdr:";

/// 需要来源校验的上报：方法按 `pane_id` 隐式定位窗格，且来自 herdr 自带集成。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReportTarget<'a> {
    pub(crate) pane_id: &'a str,
    pub(crate) source: &'a str,
}

/// 取出需要校验的上报目标；其它方法、第三方 source 与不带 source 的请求返回 `None`。
pub(crate) fn report_target(method: &Method) -> Option<ReportTarget<'_>> {
    let (pane_id, source) = match method {
        Method::PaneReportAgent(params) => (&params.pane_id, Some(&params.source)),
        Method::PaneReportAgentSession(params) => (&params.pane_id, Some(&params.source)),
        Method::PaneReportAgentActivity(params) => (&params.pane_id, Some(&params.source)),
        Method::PaneReportMetadata(params) => (&params.pane_id, Some(&params.source)),
        Method::PaneClearAgentAuthority(params) => (&params.pane_id, params.source.as_ref()),
        Method::PaneReleaseAgent(params) => (&params.pane_id, Some(&params.source)),
        _ => return None,
    };
    let source = source?;
    source
        .starts_with(BUNDLED_INTEGRATION_SOURCE_PREFIX)
        .then_some(ReportTarget { pane_id, source })
}

/// 在 API 连接线程上采下的上报来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportOrigin {
    pub(crate) peer_pid: Option<u32>,
    /// 对端的父链快照；拿不到对端 pid 或起点读不到时为 `None`。
    pub(crate) lineage: Option<ProcessLineage>,
}

/// 来源判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReportVerdict {
    /// 对端在窗格进程树里。
    InsidePane,
    /// 查不清：拿不到对端 pid、父链不完整或窗格没有根进程。放行。
    Unverifiable,
    /// 父链完整走到根也不经过窗格根进程：脱离窗格运行的进程。静默丢弃。
    Detached,
}

impl ReportVerdict {
    pub(crate) fn accepts(self) -> bool {
        !matches!(self, Self::Detached)
    }
}

impl ReportOrigin {
    /// 采下对端的父链。
    pub(crate) fn capture(peer_pid: Option<u32>) -> Self {
        Self::capture_with(peer_pid, crate::platform::process_lineage)
    }

    /// 可注入进程查询的采集：测试用假父链驱动。
    pub(crate) fn capture_with(
        peer_pid: Option<u32>,
        lineage_of: impl FnOnce(u32) -> Option<ProcessLineage>,
    ) -> Self {
        Self {
            peer_pid,
            lineage: peer_pid.and_then(lineage_of),
        }
    }

    /// 拿窗格根进程判定（事件循环上调用，只做集合查找）。
    pub(crate) fn verdict(&self, pane_root_pid: Option<u32>) -> ReportVerdict {
        let (Some(lineage), Some(root)) = (self.lineage.as_ref(), pane_root_pid) else {
            return ReportVerdict::Unverifiable;
        };
        match lineage.descends_from(root) {
            Some(true) => ReportVerdict::InsidePane,
            None => ReportVerdict::Unverifiable,
            Some(false) => ReportVerdict::Detached,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{
        PaneAgentState, PaneClearAgentAuthorityParams, PaneReportAgentParams,
        PaneReportAgentSessionParams, PaneSendTextParams,
    };
    use crate::platform::ProcessParentEntry;

    fn lineage(chain: &[(u32, &str)], complete: bool) -> ProcessLineage {
        let processes = chain
            .iter()
            .enumerate()
            .map(|(index, (pid, name))| ProcessParentEntry {
                pid: *pid,
                parent_pid: chain.get(index + 1).map_or(0, |(parent, _)| *parent),
                name: (*name).into(),
            })
            .collect();
        ProcessLineage {
            processes,
            complete,
        }
    }

    fn session_report(source: &str) -> Method {
        Method::PaneReportAgentSession(PaneReportAgentSessionParams {
            pane_id: "w1:p1".into(),
            source: source.into(),
            agent: "claude".into(),
            seq: None,
            agent_session_id: Some("s".into()),
            agent_session_path: None,
            session_start_source: None,
        })
    }

    /// 窗格 shell 20 下的前台 claude 钩子。
    fn inside_pane() -> ProcessLineage {
        lineage(
            &[
                (40, "python3"),
                (30, "claude"),
                (20, "zsh"),
                (10, "herdr"),
                (1, "systemd"),
            ],
            true,
        )
    }

    /// 取证里的后台会话：bg-pty-host → systemd --user → 1。
    fn detached() -> ProcessLineage {
        lineage(
            &[
                (930, "python3"),
                (920, "claude"),
                (910, "bg-pty-host"),
                (900, "systemd"),
                (1, "systemd"),
            ],
            true,
        )
    }

    fn origin(lineage: Option<ProcessLineage>) -> ReportOrigin {
        ReportOrigin::capture_with(Some(930), |_| lineage)
    }

    #[test]
    fn only_bundled_integration_reports_are_targets() {
        let bundled = session_report("herdr:claude");
        let target = report_target(&bundled).expect("herdr: 来源要校验");
        assert_eq!(target.pane_id, "w1:p1");
        assert_eq!(target.source, "herdr:claude");
        assert_eq!(report_target(&session_report("custom:bot")), None);
        assert_eq!(report_target(&session_report("herdr")), None);
        assert_eq!(
            report_target(&Method::PaneReportAgent(PaneReportAgentParams {
                pane_id: "w1:p1".into(),
                source: "herdr:pi".into(),
                agent: "pi".into(),
                state: PaneAgentState::Working,
                message: None,
                seq: None,
                agent_session_id: None,
                agent_session_path: None,
            }))
            .map(|target| target.source),
            Some("herdr:pi")
        );
        let clear = |source: Option<&str>| {
            Method::PaneClearAgentAuthority(PaneClearAgentAuthorityParams {
                pane_id: "w1:p1".into(),
                source: source.map(Into::into),
                seq: None,
            })
        };
        assert!(report_target(&clear(Some("herdr:codex"))).is_some());
        assert_eq!(
            report_target(&clear(None)),
            None,
            "不带 source 的清除不校验"
        );
        assert_eq!(
            report_target(&Method::PaneSendText(PaneSendTextParams {
                pane_id: "w1:p1".into(),
                text: "x".into(),
            })),
            None,
            "按显式 pane_id 操作的方法不校验"
        );
    }

    #[test]
    fn verdict_accepts_descendants_and_drops_detached_processes() {
        assert_eq!(
            origin(Some(inside_pane())).verdict(Some(20)),
            ReportVerdict::InsidePane
        );
        assert_eq!(
            origin(Some(inside_pane())).verdict(Some(40)),
            ReportVerdict::InsidePane,
            "窗格根进程自己上报（插件跑在 agent 进程内）"
        );
        let verdict = origin(Some(detached())).verdict(Some(20));
        assert_eq!(verdict, ReportVerdict::Detached);
        assert!(!verdict.accepts());
    }

    #[test]
    fn verdict_fails_open_when_the_origin_cannot_be_established() {
        let no_peer = ReportOrigin::capture_with(None, |_| Some(detached()));
        assert_eq!(no_peer.lineage, None, "没有对端 pid 就不查父链");
        assert_eq!(no_peer.verdict(Some(20)), ReportVerdict::Unverifiable);
        assert_eq!(
            origin(None).verdict(Some(20)),
            ReportVerdict::Unverifiable,
            "父链起点读不到"
        );
        let mut broken = detached();
        broken.complete = false;
        assert_eq!(
            origin(Some(broken)).verdict(Some(20)),
            ReportVerdict::Unverifiable,
            "父链中途断开"
        );
        assert_eq!(
            origin(Some(detached())).verdict(None),
            ReportVerdict::Unverifiable,
            "窗格没有根进程"
        );
        assert!(ReportVerdict::Unverifiable.accepts());
    }
}
