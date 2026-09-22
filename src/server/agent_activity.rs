//! agent 活动树的来源适配器：各 CLI 把自己的子 agent / 任务 / 待办 / 后台进程
//! 读成统一的 `AgentActivityNode`，外部来源（不属于任何 pane 的会话）另经
//! `discover_external` 给出。适配器内只有同步纯函数（入参含 `home`，可用临时
//! 目录单测）；后台线程 runtime、限频与刷新调度写在本文件，I/O 全在后台线程。

// seam-stub(activity-schema)：trait、上下文类型、注册函数与六个适配器目前没有
// 调用方。波 1 活动树 schema 车道接通 runtime 后收窄到 `SourceError` 未构造的
// 变体与 `discover_external`；波 2 各适配器落地后清零。
#![allow(dead_code)]

mod claude;
mod codex;
mod kimi;
mod opencode;
mod pi;
mod zcode;

use std::path::Path;

use crate::api::schema::{
    AgentActivityContentFormat, AgentActivityNode, ExternalAgentInfo, Method,
};

/// 一次发现 / 读取的上下文。`home` 由调用方注入，测试传临时目录。
pub(crate) struct SourceContext<'a> {
    /// 规范化的 agent 名：`"claude"`、`"codex"` 等。
    pub agent: &'a str,
    pub session: Option<&'a crate::agent_resume::AgentSessionRef>,
    pub cwd: Option<&'a Path>,
    pub home: &'a Path,
    pub now_ms: u64,
}

/// 一个节点的内容片段；`next_cursor` 对调用方不透明。
pub(crate) struct ContentChunk {
    pub format: AgentActivityContentFormat,
    pub text: String,
    pub next_cursor: Option<String>,
    pub eof: bool,
    pub truncated: bool,
}

#[derive(Debug)]
pub(crate) enum SourceError {
    /// 该来源不提供此能力（含尚未实现的适配器）。
    Unsupported,
    /// 来源此刻不可读（文件被占用、数据库被锁）；稍后重试。
    Unavailable,
    /// 来源内容不是预期格式；附说明，不 panic。
    Malformed(String),
    Io(std::io::Error),
}

pub(crate) trait ActivitySource: Send + Sync {
    /// 来源 id，与 `source_for` 的 agent 名一致。
    fn id(&self) -> &'static str;

    /// 该会话下的活动节点。缺文件 / 缺字段一律降级为空或部分结果，绝不 panic。
    fn discover(&self, cx: &SourceContext<'_>) -> Result<Vec<AgentActivityNode>, SourceError>;

    /// 读一个节点的内容片段；`cursor` 对调用方不透明。
    fn read(
        &self,
        cx: &SourceContext<'_>,
        node_id: &str,
        cursor: Option<&str>,
        max_bytes: usize,
    ) -> Result<ContentChunk, SourceError>;

    /// 仅外部来源实现；pane 型来源用默认空实现。
    fn discover_external(
        &self,
        _home: &Path,
        _now_ms: u64,
    ) -> Result<Vec<ExternalAgentInfo>, SourceError> {
        Ok(Vec::new())
    }
}

/// 按规范化 agent 名找适配器。
pub(crate) fn source_for(agent: &str) -> Option<&'static dyn ActivitySource> {
    match agent {
        "claude" => Some(&claude::Claude),
        "codex" => Some(&codex::Codex),
        "kimi" => Some(&kimi::Kimi),
        "opencode" => Some(&opencode::OpenCode),
        "pi" => Some(&pi::Pi),
        "zcode" => Some(&zcode::ZCode),
        _ => None,
    }
}

/// 会给出外部条目（不属于任何 pane 的会话）的来源。
pub(crate) fn external_sources() -> &'static [&'static dyn ActivitySource] {
    &[&zcode::ZCode]
}

/// 需要 server 上下文、由本模块处理的方法。
pub(crate) fn handles(method: &Method) -> bool {
    matches!(
        method,
        Method::AgentActivityRead(_) | Method::AgentExternalList(_)
    )
}

/// 桩应答：方法已登记但 server 侧尚未实现。
pub(crate) const NOT_IMPLEMENTED_CODE: &str = "not_implemented";
pub(crate) const NOT_IMPLEMENTED_MESSAGE: &str = "agent activity is not implemented by this server";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registered_source_answers_to_its_own_id() {
        for agent in ["claude", "codex", "kimi", "opencode", "pi", "zcode"] {
            let source = source_for(agent).expect("已预注册的来源");
            assert_eq!(source.id(), agent);
        }
        assert!(source_for("unknown-agent").is_none());
        assert_eq!(
            external_sources()
                .iter()
                .map(|source| source.id())
                .collect::<Vec<_>>(),
            ["zcode"]
        );
    }

    #[test]
    fn stub_sources_report_unsupported_without_panicking() {
        let home = std::env::temp_dir();
        for agent in ["claude", "codex", "kimi", "opencode", "pi", "zcode"] {
            let source = source_for(agent).expect("已预注册的来源");
            let cx = SourceContext {
                agent,
                session: None,
                cwd: None,
                home: &home,
                now_ms: 0,
            };
            assert!(matches!(
                source.discover(&cx),
                Err(SourceError::Unsupported)
            ));
            assert!(matches!(
                source.read(&cx, "node", None, 1024),
                Err(SourceError::Unsupported)
            ));
        }
    }

    #[test]
    fn handles_only_the_server_context_activity_methods() {
        use crate::api::schema::{AgentActivityReadParams, EmptyParams};
        assert!(handles(&Method::AgentActivityRead(
            AgentActivityReadParams::default()
        )));
        assert!(handles(&Method::AgentExternalList(EmptyParams::default())));
        assert!(!handles(&Method::AgentList(EmptyParams::default())));
    }
}
