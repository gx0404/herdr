# AGENT_RULES 使用与维护

本目录是 herdr 的 AI 领域规则库。`routes.toml` 是唯一机器真源；本 README 与根
AGENTS.md 的索引仅帮助定位，实际必读集合以路由器输出为准。

## 使用

```bash
# 任务开始时：对本轮全部触及路径解析必读规则（目录会展开，多路径取并集）
python3 scripts/resolve_agent_rules.py <path>...

# 审核任务追加 code-review 规则
python3 scripts/resolve_agent_rules.py --task review <path>...

# 机器可读输出
python3 scripts/resolve_agent_rules.py --json <path>...

# 闭集校验（体积上限、双射、零命中、覆盖、嵌套 AGENTS、重复正文）
python3 scripts/resolve_agent_rules.py --check
```

- scope 扩大后必须用完整集合重跑；resolver 无状态，不缓存上次结果。
- 未知路径 / 未登记任务 / 领域与 root_only 重叠都会以退出码 2 失败，这是有意设计。
- 尚未创建但已路由的文件可以直接传入，父目录 glob 会命中。

## 维护

- 新增仓库文件时同步 `routes.toml`：没有路由的文件会让 `--check` 失败；不想维护
  领域规则的文件才允许进 `root_only`（当前为空，不要用宽 glob 填补覆盖缺口）。
- 每份领域文档 ≤ 16384 bytes、非空、恰好登记一次；`README.md` 不参与闭集。
- 领域规则写不变量、真源符号（`模块路径::符号`）与验证方式，不钉行号，不复制
  其他文档的正文（≥160 字符的重复段落会被拒绝）；引用别的域用相对链接。
- 操作性数字只允许与源码符号同现（如 `PROTOCOL_VERSION` 的当前值），不裸写。
- `scripts/test_resolve_agent_rules.py` 是路由契约的行为测试，改 resolver 或
  routes 语义后必须保持全绿并按需补拒绝项。

## 与上游 herdrdev/herdr 的章节映射

本目录由上游单体 AGENTS.md（约 27 KiB）拆分而来。上游合并后若 AGENTS.md 冲突，
按下表把上游改动移植到对应文档（右侧为真源）：

| 上游章节 | 现在的真源 |
|---|---|
| Scope and Audience | 根 AGENTS.md「角色与授权分层」＋ `governance.md` |
| Universal Project Rules / Principles | 根「跨域硬边界」＋ `app-render.md`、`platform.md`、`detection.md` |
| Multiplicative performance paths | 根「跨域硬边界」摘要 ＋ `app-render.md` 全文 |
| Runtime/client boundary guardrail | 根摘要 ＋ `protocol-api.md` |
| Stable client endpoint contract | `protocol-api.md` |
| Maintainer Workflow | `governance.md` |
| Testing | `testing.md`（根保留一行入口） |
| Local Can Machine Workflow | `governance.md` |
| Agent Detection Updates | `detection.md` |
| Vendored libghostty-vt | `vendored-libghostty-vt.md` |
| Docs | `docs-pipeline.md` |
| Commit Style | 根 AGENTS.md「提交规范」 |
| Code Conventions | `project-infra.md`、`platform.md`、`integration-assets.md`、`protocol-api.md` |
| Release Channels | `release-channels.md` |
| External contributor guardrail | `governance.md` |

框架本身（路由、工具面、图谱、KB、文档集）是 fork 侧资产，上游没有对应物；
上游合并只可能冲突根 AGENTS.md 与既有业务文件。
