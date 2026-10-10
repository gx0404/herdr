//! harness（各家编码 agent）监控的共享模型：六家官方 harness 的声明式注册表、
//! 统一状态桶、工具调用展示、会话洞察的统一组装、活动状态归一表，以及适配器共用的
//! 文本 / JSON / 时间工具、有界尾读缓存与有界子进程。server 与 TUI 客户端共用。

pub mod bucket;
pub mod insight;
pub mod json;
pub mod registry;
pub mod status;
pub mod subprocess;
pub mod tail;
pub mod text;
pub mod time;
pub mod tool_display;

pub use bucket::AgentBucket;
