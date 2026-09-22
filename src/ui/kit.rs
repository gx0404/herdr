//! 设计系统原语的落点：`gauge`、`meter_row`、`braille_chart`、`card`、`tabs`、
//! `form_field`、`tree`、`menu`、`hover_card`、`footer_hints`、`empty_state`、
//! `table` 各占一个子模块（`src/ui/kit/<name>.rs`），由原语车道在这里逐个声明。
//!
//! 统一约定：自由函数；**只画不改状态**；可交互原语返回命中矩形，由调用方写进
//! 自己的命中表；热路径不分配；`ascii` / `glyphs` 开关做字形降级。本目录同时
//! 服务 server 直连渲染，不得反向依赖 `crate::client`。调用写全路径
//! （`crate::ui::kit::gauge::render_gauge`），不往 `crate::ui` 的 re-export 区堆符号。
