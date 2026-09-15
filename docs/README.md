# herdr 贡献者文档索引

面向在仓库内工作的人类与 agent。用户产品文档在 `docs/next/website/`（发布管线
见 `AGENT_RULES/docs-pipeline.md`）。AI 领域规则在 `docs/AGENT_RULES/`，其机器
真源是 `AGENT_RULES/routes.toml`（用 `python3 scripts/resolve_agent_rules.py
<paths...>` 解析必读集合）。

| 文档 | 内容 |
|---|---|
| `ARCHITECTURE.md` | 进程模型、src 模块地图、状态所有者、数据流 |
| `DEVELOPMENT.md` | 环境准备与开发交付闭环 |
| `MAKE_COMMANDS.md` | 全部 `just` 命令：用途、前置、副作用、证据 |
| `TESTING.md` | 测试分层、入口、失败路径与证据要求 |
| `AI_TOOLS.md` | AI 工具接入矩阵与逐工具验证账 |
| `AGENT_RULES/` | 16 个领域规则 + 路由（README 讲用法与维护） |
| `kb/` | agent 知识库产物（`chunks.json`，生成物，勿手改） |
| `graphify/` | 代码图谱报告镜像（与 `graphify-out/GRAPH_REPORT.md` 字节一致） |

## 生成物登记

默认只检查，有意变更才重建并审 diff；Freshness 门失败不是「重跑生成器盖掉」，
而是先确认输入变化是否真的有意。

| 产物 | 输入 | 生成器 | 消费者 | 检查 |
|---|---|---|---|---|
| `docs/next/website/src/data/config-reference.json` | `src/config` 模型 | 内部生成 | 网站 config-reference 页 | `python3 scripts/config_reference_check.py`（release-docs-check 内） |
| `docs/next/api/herdr-api.schema.json` | `src/api/schema`（schemars） | cargo 测试/构建期 | socket API 文档与客户端 | `just test`（schema 测试） |
| `distribution/agent-detection/*.toml` | `src/detect/manifests/*.toml` | 复制 + index | 稳定客户端运行时下载 | `python3 scripts/agent_detection_manifest_check.py`（maintenance-test） |
| `src/ghostty/bindings.rs` | vendored C API | `just libghostty-bindings`（bindgen-cli 0.72.1） | `src/ghostty` | 编译本身 + `test_vendor_libghostty_vt` |
| `docs/kb/chunks.json` | 上述文档、manifest、config 契约、`src/**/*.rs` 结构 | `scripts/build_agent_kb.py` | `scripts/agent_kb.py` 检索 | `just kb-check` + `scripts/test_agent_kb.py` golden |
| `graphify-out/GRAPH_REPORT.md`（镜像 `docs/graphify/`）、标签+sig、指纹 | `src/**`（Rust） | `scripts/graphify.sh rebuild` | 图查询、overview | `just graph-check` |
| `CHANGELOG.md` / `docs/versions/<v>/` / `distribution/latest.json` | `docs/next` + tag | release 流程（`just release*`、release.yml） | 用户/更新器/网站 | `just release-docs-check`、distribution.yml |

约定：新增生成物时在本表登记输入→生成器→产物→消费者→检查，并默认提供只读
check 与显式写入两个入口（详见 `AGENT_RULES/testing.md`、`ai-tooling.md`）。
