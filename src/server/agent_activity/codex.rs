//! Codex CLI 的活动来源适配器：子任务与计划项。
//!
//! seam-stub(adapter-codex)：波 2 适配器车道在本文件内实现；接缝只预注册空壳，
//! 注册表（`super::source_for` / `super::external_sources`）不需要再改。

use super::{ActivitySource, ContentChunk, SourceContext, SourceError};
use crate::api::schema::AgentActivityNode;

pub(super) struct Codex;

impl ActivitySource for Codex {
    fn id(&self) -> &'static str {
        "codex"
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
