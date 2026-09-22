//! Claude Code 的活动来源适配器：子 agent（Task 工具）、待办与后台 shell。
//!
//! seam-stub(adapter-claude)：波 2 适配器车道在本文件内实现；接缝只预注册空壳，
//! 注册表（`super::source_for` / `super::external_sources`）不需要再改。

use super::{ActivitySource, ContentChunk, SourceContext, SourceError};
use crate::api::schema::AgentActivityNode;

pub(super) struct Claude;

impl ActivitySource for Claude {
    fn id(&self) -> &'static str {
        "claude"
    }

    fn discover(&self, _cx: &SourceContext<'_>) -> Result<Vec<AgentActivityNode>, SourceError> {
        Err(SourceError::Unsupported)
    }

    fn read(
        &self,
        _cx: &SourceContext<'_>,
        _node_id: &str,
        _cursor: Option<&str>,
        _max_bytes: usize,
    ) -> Result<ContentChunk, SourceError> {
        Err(SourceError::Unsupported)
    }
}
