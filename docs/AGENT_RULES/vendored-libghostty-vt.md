# vendored-libghostty-vt（vendored 终端内核与补丁）

范围：`vendor/**`（libghostty-vt 源码树、portable-pty 补丁副本）、vendoring 与
绑定生成脚本。`vendor/` 内的上游文件（含其自带 AGENTS.md/AI_POLICY.md）是上游
资产，不修改、不重排。

## 真源与索引（上游全文语义）

- `vendor/libghostty-vt.vendor.json` 记录当前 vendored 的上游源码 commit。
- 本地补丁必须登记在 `vendor/libghostty-vt.patches.md` 并以补丁文件存于
  `vendor/patches/libghostty-vt/`。每个条目写清：补丁存在原因、herdr issue、
  上游 PR/讨论、vendored 基线 commit、触及文件、验证方式与确切移除条件。

## 更新流程（上游全文语义）

更新 libghostty-vt 时逐个检查 `vendor/libghostty-vt.patches.md` 的活跃补丁：
新上游 commit 已含修复 → 删除本地补丁与索引条目并重跑条目所列验证；未含 →
在新 vendored 源码上重放补丁。

`just check` 的 maintenance 测试校验本地补丁文件已登记进索引并能对 vendored 树
反向干净应用；不得留下未跟踪补丁文件或已登记未应用的补丁。

## 绑定与构建

- `scripts/build_vendored_libghostty_vt.sh` 构建源码分发包；`scripts/
  generate_libghostty_bindings.sh`（`just libghostty-bindings`，bindgen-cli
  0.72.1）再生成 C API 绑定。顺序不可颠倒，绑定产物入库。
- Zig 版本是构建输入（当前要求 0.16.0；Windows VM 场景见 `governance.md`）；
  绑定层使用纪律见 `terminal-core.md`。
- `vendor/portable-pty` 经 `[patch.crates-io]` 指向；其测试
  （`scripts/test_vendor_portable_pty.py`）随 maintenance-test 运行。
