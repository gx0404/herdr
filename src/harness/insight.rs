//! 会话洞察的统一组装：各家适配器把自己会话里的记录按时间顺序喂给 [`InsightScan`]，
//! 判定规则只在这里写一份，适配器只管解析各自的格式。
//!
//! - 文本一律压成单行、去掉 markdown（[`super::text`]）：最后回复、最后提示、待答问题
//!   与错误最多 [`MESSAGE_CHARS`] 个字符；会话标题、模型、当前工具与回答选项最多
//!   [`LABEL_CHARS`] 个字符，选项最多 [`MAX_OPTIONS`] 个。
//! - 一条真正的用户提示开启新回合：上一回合的错误、待答问题与没有结果的工具调用一并
//!   作废。回合中途补充的用户输入（steer）只更新最后提示。
//! - 错误只属于它所在的回合：之后又有用户提示、或 agent 又给出了正常回复，错误作废。
//!   错误带上它自己的时刻（`AgentInsight::error_at_ms`）；server 只用晚于本回合开始的
//!   错误锁存失败，没有时刻的错误只展示、不锁存。
//! - 当前工具是最近开始、还没有结果的工具调用；回合结束时清空。
//! - `updated_at_ms` 取参与洞察的最新记录的时刻，不取读取时刻：会话没有变化时洞察
//!   逐字节相同，快照不必重建。

use crate::api::schema::AgentInsight;

use super::text::{clean_line, summary_text};
use super::tool_display::ToolDisplay;

/// 最后回复、最后提示、待答问题与错误的字符上限。
pub const MESSAGE_CHARS: usize = 220;
/// 会话标题、模型、当前工具与回答选项的字符上限。
pub const LABEL_CHARS: usize = 120;
/// 回答选项的个数上限。
pub const MAX_OPTIONS: usize = 8;

#[derive(Debug)]
struct OpenTool {
    call_id: String,
    line: String,
}

#[derive(Debug)]
struct Question {
    /// 提问所属的工具调用；工具有了结果（用户已回答）时问题作废。
    call_id: Option<String>,
    text: String,
    options: Vec<String>,
}

/// 按时间顺序累积一个会话的洞察（规则见模块文档）。
#[derive(Debug, Default)]
pub struct InsightScan {
    title: Option<String>,
    model: Option<String>,
    prompt: Option<String>,
    message: Option<String>,
    tools: Vec<OpenTool>,
    question: Option<Question>,
    error: Option<(String, Option<u64>)>,
    updated_at_ms: Option<u64>,
}

impl InsightScan {
    /// 会话标题；空白的不覆盖已有的。
    pub fn title(&mut self, title: &str) {
        if let Some(title) = clean_line(title, LABEL_CHARS) {
            self.title = Some(title);
        }
    }

    /// 当前使用的模型；空白的不覆盖已有的。
    pub fn model(&mut self, model: &str) {
        if let Some(model) = clean_line(model, LABEL_CHARS) {
            self.model = Some(model);
        }
    }

    pub fn has_model(&self) -> bool {
        self.model.is_some()
    }

    /// 一条真正的用户提示：开启新回合。
    pub fn prompt(&mut self, text: &str, at_ms: Option<u64>) {
        let Some(prompt) = summary_text(text, MESSAGE_CHARS) else {
            return;
        };
        self.prompt = Some(prompt);
        self.error = None;
        self.question = None;
        self.tools.clear();
        self.touch(at_ms);
    }

    pub fn has_prompt(&self) -> bool {
        self.prompt.is_some()
    }

    /// 回合中途补充的用户输入：只更新最后提示。
    pub fn steer(&mut self, text: &str, at_ms: Option<u64>) {
        if let Some(prompt) = summary_text(text, MESSAGE_CHARS) {
            self.prompt = Some(prompt);
            self.touch(at_ms);
        }
    }

    /// agent 的一段正常回复：更新最后回复，同一回合里更早的错误作废。
    pub fn message(&mut self, text: &str, at_ms: Option<u64>) {
        let Some(message) = summary_text(text, MESSAGE_CHARS) else {
            return;
        };
        self.message = Some(message);
        self.error = None;
        self.touch(at_ms);
    }

    /// 一次工具调用开始；同一调用 id 再次出现时以最后一次为准。
    pub fn tool_started(&mut self, call_id: &str, display: &ToolDisplay, at_ms: Option<u64>) {
        let Some(line) = clean_line(&display.line(), LABEL_CHARS) else {
            return;
        };
        self.tools.retain(|tool| tool.call_id != call_id);
        self.tools.push(OpenTool {
            call_id: call_id.to_owned(),
            line,
        });
        self.touch(at_ms);
    }

    /// 一次工具调用有了结果；它带来的问题随之作废。
    pub fn tool_finished(&mut self, call_id: &str, at_ms: Option<u64>) {
        let before = self.tools.len();
        self.tools.retain(|tool| tool.call_id != call_id);
        let question = self
            .question
            .as_ref()
            .is_some_and(|question| question.call_id.as_deref() == Some(call_id));
        if question {
            self.question = None;
        }
        if question || self.tools.len() != before {
            self.touch(at_ms);
        }
    }

    /// agent 已经往下走（开始了新一轮回复），之前的工具调用必然都有了结果。
    pub fn tools_settled(&mut self) {
        self.tools.clear();
        if self
            .question
            .as_ref()
            .is_some_and(|question| question.call_id.is_some())
        {
            self.question = None;
        }
    }

    /// agent 正在问用户的问题；`call_id` 是提问的工具调用（交互请求的 id 同样可以）。
    pub fn question(
        &mut self,
        call_id: Option<&str>,
        text: &str,
        options: &[String],
        at_ms: Option<u64>,
    ) {
        let Some(text) = summary_text(text, MESSAGE_CHARS) else {
            return;
        };
        self.question = Some(Question {
            call_id: call_id.map(str::to_owned),
            text,
            options: options
                .iter()
                .filter_map(|option| clean_line(option, LABEL_CHARS))
                .take(MAX_OPTIONS)
                .collect(),
        });
        self.touch(at_ms);
    }

    /// 某个提问已有回答。
    pub fn question_answered(&mut self, call_id: &str, at_ms: Option<u64>) {
        if self
            .question
            .as_ref()
            .is_some_and(|question| question.call_id.as_deref() == Some(call_id))
        {
            self.question = None;
            self.touch(at_ms);
        }
    }

    /// 会话里的一次错误（API 报错、回合失败）；`at_ms` 是错误记录自己的时刻。
    pub fn error(&mut self, text: &str, at_ms: Option<u64>) {
        if let Some(error) = summary_text(text, MESSAGE_CHARS) {
            self.error = Some((error, at_ms));
            self.touch(at_ms);
        }
    }

    /// 回合结束（完成、失败或被中断）：没有结果的工具调用与问题都不再进行。
    pub fn turn_ended(&mut self, at_ms: Option<u64>) {
        if !self.tools.is_empty() || self.question.is_some() {
            self.tools.clear();
            self.question = None;
            self.touch(at_ms);
        }
    }

    fn touch(&mut self, at_ms: Option<u64>) {
        if let Some(at_ms) = at_ms {
            self.updated_at_ms = Some(self.updated_at_ms.map_or(at_ms, |seen| seen.max(at_ms)));
        }
    }

    /// 组装结果；什么都没读到时为 `None`。
    pub fn finish(self) -> Option<AgentInsight> {
        let current_tool = self.tools.last().map(|tool| tool.line.clone());
        let (question, options) = match self.question {
            Some(question) => (Some(question.text), question.options),
            None => (None, Vec::new()),
        };
        let (error, error_at_ms) = match self.error {
            Some((error, at_ms)) => (Some(error), at_ms),
            None => (None, None),
        };
        let empty = self.title.is_none()
            && self.model.is_none()
            && self.prompt.is_none()
            && self.message.is_none()
            && current_tool.is_none()
            && question.is_none()
            && error.is_none();
        (!empty).then_some(AgentInsight {
            session_title: self.title,
            model: self.model,
            current_tool,
            last_message: self.message,
            last_prompt: self.prompt,
            question,
            options,
            error,
            error_at_ms,
            updated_at_ms: self.updated_at_ms,
            ..AgentInsight::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::tool_display::display;
    use serde_json::json;

    fn shell(command: &str) -> ToolDisplay {
        display("Bash", Some(&json!({ "command": command })), None)
    }

    #[test]
    fn an_empty_scan_has_no_insight() {
        assert_eq!(InsightScan::default().finish(), None);
        let mut scan = InsightScan::default();
        scan.prompt("   ", Some(1));
        scan.title("\n");
        assert_eq!(scan.finish(), None, "空白文本不算读到");
    }

    #[test]
    fn texts_are_single_line_without_markdown_and_capped() {
        let mut scan = InsightScan::default();
        scan.title("  Fix\nfocus  ");
        scan.model("opus");
        scan.prompt("Please **fix** the `focus` bug", Some(10));
        scan.message(&format!("## Done\n{}", "x".repeat(400)), Some(20));
        let insight = scan.finish().expect("读到了洞察");
        assert_eq!(insight.session_title.as_deref(), Some("Fix focus"));
        assert_eq!(
            insight.last_prompt.as_deref(),
            Some("Please fix the focus bug")
        );
        let message = insight.last_message.expect("最后回复");
        assert!(message.starts_with("Done x"));
        assert_eq!(message.chars().count(), MESSAGE_CHARS);
        assert!(message.ends_with('…'));
        assert_eq!(insight.updated_at_ms, Some(20));
    }

    #[test]
    fn the_current_tool_is_the_latest_call_without_a_result() {
        let mut scan = InsightScan::default();
        scan.tool_started("a", &shell("cargo build"), Some(1));
        scan.tool_started("b", &shell("cargo test"), Some(2));
        scan.tool_finished("b", Some(3));
        let insight = scan.finish().expect("洞察");
        assert_eq!(insight.current_tool.as_deref(), Some("Shell cargo build"));
        assert_eq!(insight.updated_at_ms, Some(3));

        let mut scan = InsightScan::default();
        scan.tool_started("a", &shell("cargo build"), Some(1));
        scan.tools_settled();
        scan.message("built", Some(2));
        assert_eq!(scan.finish().and_then(|insight| insight.current_tool), None);

        let mut scan = InsightScan::default();
        scan.tool_started("a", &shell("sleep 9"), Some(1));
        scan.turn_ended(Some(2));
        assert_eq!(
            scan.finish(),
            None,
            "回合结束后没有进行中的工具，其余也什么都没有"
        );
    }

    #[test]
    fn a_new_prompt_or_a_later_reply_retires_the_previous_error() {
        let mut scan = InsightScan::default();
        scan.prompt("first", Some(1));
        scan.error("API Error: 529 overloaded", Some(2));
        let insight = scan.finish().expect("洞察");
        assert_eq!(insight.error.as_deref(), Some("API Error: 529 overloaded"));
        assert_eq!(insight.error_at_ms, Some(2), "错误带它自己的时刻");

        let mut scan = InsightScan::default();
        scan.error("API Error: 529 overloaded", Some(2));
        scan.prompt("try again", Some(3));
        let insight = scan.finish().expect("洞察");
        assert_eq!((insight.error, insight.error_at_ms), (None, None));

        let mut scan = InsightScan::default();
        scan.error("stream dropped", Some(2));
        scan.message("recovered and finished", Some(3));
        assert_eq!(scan.finish().and_then(|insight| insight.error), None);

        let mut scan = InsightScan::default();
        scan.error("no time on this record", None);
        let insight = scan.finish().expect("洞察");
        assert_eq!(insight.error_at_ms, None, "没有时刻的错误不编造时刻");
    }

    #[test]
    fn questions_follow_their_tool_call() {
        let options = ["Yes".to_owned(), " ".to_owned(), "No".to_owned()];
        let mut scan = InsightScan::default();
        scan.question(Some("q1"), "Retry on **429**?", &options, Some(5));
        let insight = scan.finish().expect("洞察");
        assert_eq!(insight.question.as_deref(), Some("Retry on 429?"));
        assert_eq!(insight.options, ["Yes", "No"]);

        let mut scan = InsightScan::default();
        scan.question(Some("q1"), "Retry?", &options, Some(5));
        scan.tool_finished("q1", Some(6));
        assert_eq!(scan.finish(), None, "回答之后问题作废");

        let mut scan = InsightScan::default();
        scan.question(Some("i1"), "Retry?", &options, Some(5));
        scan.question_answered("other", Some(6));
        assert!(scan
            .finish()
            .is_some_and(|insight| insight.question.is_some()));

        let many: Vec<String> = (0..12).map(|index| format!("option {index}")).collect();
        let mut scan = InsightScan::default();
        scan.question(None, "Pick one", &many, None);
        assert_eq!(
            scan.finish().map(|insight| insight.options.len()),
            Some(MAX_OPTIONS)
        );
    }

    #[test]
    fn steering_keeps_the_running_turn() {
        let mut scan = InsightScan::default();
        scan.prompt("build it", Some(1));
        scan.tool_started("a", &shell("cargo build"), Some(2));
        scan.steer("also run the tests", Some(3));
        let insight = scan.finish().expect("洞察");
        assert_eq!(insight.last_prompt.as_deref(), Some("also run the tests"));
        assert_eq!(insight.current_tool.as_deref(), Some("Shell cargo build"));
    }
}
