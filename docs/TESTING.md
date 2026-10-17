# herdr 测试分层

## 入口与范围

| 入口 | 范围 | 边界 |
|---|---|---|
| `just test` | 固定默认回归：Rust、维护脚本、UI 热路径、集成资产、文档契约 | 热缓存端到端目标 60 秒，扩展集未执行 |
| `just test-full` | 默认集与所有扩展自动测试；Rust 全部 nextest 隔离 | 不隐含 ignored 性能、真实桌面输入或安装资格 |
| `just check` | fmt、clippy、默认回归、平台编译检查 | Windows 还构建普通二进制；不受 60 秒目标约束 |
| `just check-full` | lint、完整自动回归、平台编译检查 | 测试框架变更、广泛回归和发布准备使用 |
| `just test-one <filter>` / `just nextest-all` | 指定 Rust 集 / 完整 nextest | 每测试独立进程，不受默认集排除项限制 |
| `just maintenance-test` | Python 自动收集、顶层 Bun 契约、fork 丢弃路径检查 | `HERDR_TEST_SUITE=full` 包含扩展项 |
| `just integration-assets-test` / `just docs-contract-test` | 对应目录 Bun 自动收集 | 分类同上，不代证真实 CLI 兼容 |
| `just framework-check` | resolver、hook 探针、KB/图谱 freshness | 同步检查不代证文档语义和真实会话接入 |

## 分类真源与调度

`.config/test-suites.toml` 由 `scripts/run_test_catalog.py` 消费，登记扩展项、原因与
进程隔离项。新 Rust 单测与 `tests/*.rs`、Python `scripts/test_*.py`、对应目录的
Bun `*.test.ts` 默认纳入；UI 热路径 Python 模块由独立阶段执行一次。
失效选择器、意外空集合和必要工具缺失必须失败，不缓存测试成功结果。

默认 Rust 使用 Cargo 原生线程单测；进程资源数量和初始全局 ID 断言仍用 nextest
独立执行。完整集保留 nextest 逐测试隔离。已分类的真实 server/SSH/PTY/安装二进制、
真实 deadline、大体积响应、发布环境演练和重试定时器场景移入扩展集，断言不变。
协议、身份/旧响应、持久化失败、配置写入、输入、检测、UI 几何和集成契约仍有默认覆盖。

Python 按类分组、按历史耗时从长到短调度。历史只影响顺序，不决定选中集合。
五阶段默认并发 `min(3, 5, budget)`，Rust/Python 合并预算默认 `min(8, cpu_count)`；
worker 与命令行覆盖语义见 `AGENT_RULES/testing.md`。Bun 阶段也有进程/计时器成本，
合并预算不是整机 CPU 硬限额。完整 nextest 可显式分片，默认线程单测不分片。

Linux/macOS/Windows CI 共用分类。push/PR 使用默认集；现有 CI workflow 手动输入
`full_tests` 选择完整集。macOS 保留既有 live_handoff 过滤，专项资格可另补跑；
ghostty、平台编译和 ConPTY 验收保留各自范围。归档 workflow 不执行。
GX 安装包 workflow 显式设置 `HERDR_TEST_SUITE=full`，发布前不使用精简默认集。

## 证据与 60 秒验收

`target/test-suite-logs/<run-id>/manifest.json` 记录阶段状态、退出码、日志和 worker，
新增 `suite`、`total_seconds`、`target_seconds`、`target_met`、`extended_tests` 与
实际执行/跳过计数。未知计数为 null，pending/running/cancelled 均不是通过。
超出 60 秒继续完成，不截断、不伪报达标；默认通过与完整通过分别输出。

在当前机器、相同 worker 配置和最终代码上预热后连续三次 `just test` 均 ≤60 秒才能
标记达标；另记录一次真实源码小改后的增量编译运行。计时含发现、增量编译和汇总。
冷构建及 `just check` 单独记录，旧版本测量不代表当前版本。

## 隔离与专项验证

- 使用 `python -B scripts/setup_env.py --run just test` 隔离用户配置、HOME/AppData/XDG、
  临时目录和缓存；已有 Python/Rust/Git 可只读复用。测试子进程使用当前解释器，避开安装器别名。
- 拒绝门须覆盖允许与拒绝。fixture 不为过门修改，freshness 失败先确认输入变化。
- 守护进程需进程外回收与陈旧沙箱清扫，不能依赖 Drop 在强杀后执行。
- TUI 需要视觉验收：`just ui-gallery` 的 compose HTML/PNG 必须读回；真实输入、IME、
  剪贴板和 PTY 另用隔离会话及实机验证。纯编排改动不要求重画 UI。
- 真实 agent 兼容、安装升级、发布性能和 Windows 输入资格独立运行；缺环境记 PENDING。
