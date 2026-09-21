# detection（agent 状态检测与 manifest）

范围：`src/detect/**`（含 `manifests/`）、`src/pane/agent_detection.rs`、
`src/server/autodetect.rs`、`tests/auto_detect.rs`、`distribution/agent-detection/**`
及相关脚本。

## 架构不变量（上游 Principles）

- **检测解耦**：detector 读屏幕快照，绝不触碰 parser 或 viewport 状态。
- **检测基于证据**：改 `src/detect/manifests/` 前，先用 `herdr agent read
  <pane> --source detection --format text` 抓取相关 bottom-buffer 状态；样式或
  alternate screen 行为相关时加 `--format ansi`。区分哪些可见控件是不变量、
  哪些是备选项，编码为显式 AND/OR 门；不匹配整 pane 偶发文本；用户可滚动的
  可见 viewport 不作为 agent 状态来源。

## Manifest 更新流程（上游全文语义）

使用 manifest 热重载环验证，不要拍脑袋改正则：

1. 用项目本地 `herdr-throwaway-repro` skill 建一次性命名会话，经 Herdr CLI/API
   驱动真实 agent UI 到目标状态。
2. `herdr agent read <pane> --source detection --format text` 读取；
   `herdr agent explain <pane> --json` 检查匹配。
3. 更新捆绑 manifest `src/detect/manifests/<agent>.toml`；复制到本地覆盖路径
   `~/.config/herdr/agent-detection/<agent>.toml`（写覆盖前先检查是否已存在，
   未经对齐不覆盖/不删除既有覆盖）。
4. 对受测会话执行 `herdr server reload-agent-manifests` 验证。
5. 规则正确后删除临时覆盖或精确还原旧覆盖，保证入库的捆绑 manifest 是真源。

不要为日常 manifest 调优添加大体积 agent 专属整屏 fixture 套件；Rust 测试聚焦
manifest 解析、规则语义、skip-state 语义、来源优先级、缓存重载与更新流程；
agent 专属屏幕证据用真实 pane 读取。

## 发布目录

`distribution/agent-detection/` 是已发布客户端的远端目录。已发布 agent 的改动
必须与其捆绑 manifest 对齐，除非 validator 记录了精确的版本+摘要例外。当前
stable 客户端无法识别的新捆绑 agent 可暂不发布（挂在精确例外后），但必须在
首个携带它的 stable 发布前加入目录并移除例外；`just release-docs-check` 强制
不存在未发布例外。`scripts/agent_detection_manifest_check.py`（`just
maintenance-test` 内）校验捆绑与发布副本一致性。

官方集成仅六家：其余 agent 的捆绑 manifest 与 `src/detect/mod.rs::Agent` 变体已
删除（`Agent` 不派生 serde、不进 wire 结构，对外只以 `agent_label` 字符串出现），
不新增，同步上游时也不合入，发布目录里的对应副本随之删减；名单与口径见
`README.md` 的「fork 已删除的集成」，目录归属的 fork 例外登记在
`release-channels.md`。

删除后的兼容不变量：

- **远程目录回流**：上游发布目录仍会列出已删 agent。
  `src/detect/manifest_update.rs::parse_catalog` 对未知 id 只 `warn` 并跳过，整份
  目录照常接受，不进状态、不落缓存、不写本地覆盖。
- **用户配置**：`src/detect/mod.rs::RETIRED_AGENT_LABELS` 登记删除前的规范 id，只
  服务配置兼容，不参与识别。`src/config/sidebar.rs::deserialize_rows_by_agent` 据此
  忽略并告警已删 agent 的 `rows_by_agent` 键（否则启动会整份回退默认、热重载会
  拒收整个 ui 段）；`src/config/sound.rs::AgentSoundOverrides` 的旧键按未知键处理，
  只产生 `unknown config key` 诊断。上游日后新增、fork 不收的 agent 不需要登记。
- **残留 hook 与旧快照**：`src/agent_resume.rs::is_official_agent_source` 只认五家，
  已删集成残留 hook 的会话上报与旧快照里的会话一律按非官方来源丢弃。
