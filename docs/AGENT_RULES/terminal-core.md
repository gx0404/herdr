# terminal-core（终端核心、PTY 与输入）

范围：`src/terminal/**`、`src/pty/**`、`src/ghostty/**`、`src/kitty_graphics*`、
`src/input/**`、`src/raw_input.rs`、`src/terminal_modes.rs`、`src/terminal_notify.rs`。

## 终端核心纪律

- 终端状态（`src/terminal/state.rs`）与运行时（`runtime.rs`、
  `runtime_registry.rs`）分离；解析器不持有呈现状态——检测只读屏幕快照
  （`detection.md`），渲染只消费 `compute_view` 结果（`app-render.md`）。
- PTY 解析是逐字节乘法路径：解析循环内禁止分配密集操作、锁长持有与进程树/
  文件系统访问（`app-render.md` 的频率×基数分析适用）。
- `src/ghostty/**` 是 vendored libghostty-vt 的绑定层：语义问题先看上游补丁
  状态（`vendored-libghostty-vt.md`），不在绑定层复刻终端逻辑。
- 输入编码/解析（`src/input/encode.rs`、`parse.rs`、`src/pane/kitty_keyboard.rs`
  等与 pane 域并集）保持 platform-gated：Windows VT 输入路径见 `platform.md`。

## kitty graphics 与 pane surface

- `src/kitty_graphics*` 与 `src/pane_graphics_files.rs`（协议域并集）处理图形
  传输协议的宿主侧；surface 生命周期与重连后的 stale surface 拒绝由
  server/client 协议层守护（`protocol-api.md`）。
- 键盘变异与终端探测语料（`tests/fixtures/` 的 keyboard/terminal TSV、
  `scripts/capture_keys.py`、`capture_key_matrix.py`）是实测采集的受控输入：
  更新语料必须注明采集环境，不手工编造序列；failures 保留现场。

## 通知与模式

`terminal_notify.rs`/`terminal_modes.rs` 的 OSC/模式变化是共享 runtime 事实，
经事件路径暴露（`protocol-api.md` 分类），不新增 TUI 私有通道。
