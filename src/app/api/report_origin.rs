//! 集成上报的来源判定（事件循环侧）：API 连接线程已把对端父链快照下来，这里只拿
//! 窗格根进程做集合查找，不做任何进程查询。规则见 `crate::api::report_origin`。

use crate::api::schema::{Method, ResponseResult};
use crate::api::ReportOrigin;

use super::responses::encode_success;
use super::App;

impl App {
    /// 来自窗格进程树之外的 herdr 集成上报：返回替它回的成功应答（静默丢弃，不改任何
    /// 状态），调用方直接回给客户端。其余情形返回 `None`，请求照常处理——包括窗格
    /// 不存在（交给常规路径报 `pane_not_found`）与查不清来源（放行）。
    pub(crate) fn detached_report_response(
        &self,
        id: &str,
        method: &Method,
        origin: &ReportOrigin,
    ) -> Option<String> {
        if !self.verify_report_process {
            return None;
        }
        let target = crate::api::report_target(method)?;
        let (ws_idx, pane_id) = self.parse_pane_id(target.pane_id)?;
        let pane_root_pid = self
            .lookup_runtime_sender(ws_idx, pane_id)
            .and_then(|runtime| runtime.child_pid());
        if origin.verdict(pane_root_pid).accepts() {
            return None;
        }
        tracing::debug!(
            pane_id = target.pane_id,
            source = target.source,
            peer_pid = origin.peer_pid,
            "丢弃来自窗格进程树之外的集成上报"
        );
        Some(encode_success(id.to_string(), ResponseResult::Ok {}))
    }
}
