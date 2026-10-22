# .zcode/

ZCode 读取根 `AGENTS.md`，领域规则经 resolver 显式读取。
`plans/`、`tmp/` 是本机状态（根 `.gitignore`）。

2026-10-22 核对[官方 hooks 文档](https://zcode.z.ai/en/docs/hooks)：当前版本整体忽略
项目 `.zcode/config.json` / `zcode.json` 的 hooks，即使 `hooks.enabled` 为 true。
因此仓库不注册项目 hooks；原有 Bash 字符串探针只证明共享脚本行为，不能证明 ZCode 接入。

需要 hooks 时，使用者可另外手动配置用户 `~/.zcode/cli/config.json` 或启用可信插件。
本仓库不写用户配置、不安装插件。手动安装需按官方协议设置 `hooks.enabled: true`，
为 PreToolUse/PostToolUse/Stop 分别引用本仓库 `.claude/hooks/` 中的
`block_dangerous.sh`、`record_edit.sh`、`notify_review.sh`；保留已有用户设置。
使用实际可执行路径，核对 shell、工作目录与 `timeoutMs` 毫秒单位；`process` 的
`command`/`args` 与兼容 `command` 执行器按官方说明选择，不复制危险模式清单。
新会话中确认实际来源与事件触发，再用无副作用输入验证允许/拒绝；脚本不能识别所有
shell 形式，覆盖边界见 `docs/AI_TOOLS.md`。手动安装和客户端调用均为 PENDING。
