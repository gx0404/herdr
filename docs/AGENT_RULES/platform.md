# platform（平台隔离与 Windows 工具链）

范围：`src/platform/**`、Windows 专属实现（与所属领域并集）、Windows 交叉编译
与 ConPTY 打包脚本。

## 平台隔离（上游 Principles + Code Conventions 全文语义）

- OS 专属行为只存在于对应的 `src/platform/<os>.rs`；`src/platform/mod.rs` 只放
  共享 trait、类型、包装与可测试契约。核心模块不得出现 `#[cfg(target_os)]`。
- 平台专属代码必须编译门控：在 `src/platform/` 之外需要平台判断时，用
  `#[cfg(windows)]`、`#[cfg(unix)]` 或目标专属 `#[cfg(...)]` 加在 import、字段、
  函数、impl 与 match 臂上，使 Windows-only 代码不编译进 Unix 构建、反之亦然。
- `cfg!(...)` 只用于两个分支在所有目标都能编译的纯跨平台策略常量。
- 平台实现的可测试契约（配置文件定位、clipboard、输入路径）在 `mod.rs` 层抽象，
  配套 `*_tests.rs` 就近维护；Windows 分支由 `just windows-lint` 与 CI 的
  Windows job 守护（见 `testing.md`）。

## Windows 交叉编译（上游 Testing 节语义）

Unix 上做 Windows MSVC 交叉编译需要 SDK/CRT 头与库：`cargo install xwin
--locked` 安装 xwin，运行一次 `just setup-windows-cross` 并接受 Microsoft SDK
许可（直接从 Microsoft 下载，无需 Windows 机器）。SDK 与 Zig libc 配置位于
`~/.local/share/herdr/windows-cross/`，worktree 共享；`just windows-lint` 与
`just check` 的 Windows 阶段自动使用。换 SDK 时设 `LIBGHOSTTY_VT_WINDOWS_LIBC`
指向其 Zig libc 配置文件。setup 支持 `--accept-license` 非交互接受；常规检查
不下载 SDK。原生 Windows 构建自动探测已装 SDK；原生 Linux/macOS 构建不需要。

## ConPTY 打包

- `scripts/package_windows_conpty.py`（.ps1 包装）产出的 app-local ConPTY 运行时
  是 Windows 发布资产的一部分（形态要求见 `release-channels.md`）；
  `scripts/windows_conpty_enhanced_input_probe.ps1` 等探针脚本验证输入路径。
- VT 输入相关行为（`src/client/input/windows_vti.rs`、
  `src/pane/terminal/windows_recent_fallback.rs`）的语义归 `terminal-core.md`
  与本域并集：先满足平台隔离，再满足所属领域不变量。
