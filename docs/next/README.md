# herdr


<p align="center">
  <img src="assets/logo.png" alt="herdr" width="100" />
</p>

<p align="center">
  <a href="https://herdr.dev">herdr.dev</a> · <a href="#install">install</a> · <a href="https://herdr.dev/docs/quick-start/">quick start</a> · <a href="https://herdr.dev/docs/">docs</a>
</p>

<p align="center">
  English · <a href="README.zh-CN.md">简体中文</a>
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-666666?labelColor=333333" alt="Apache 2.0 license" /></a>
  <a href="https://github.com/herdrdev/herdr/releases"><img src="https://img.shields.io/github/downloads/herdrdev/herdr/total?labelColor=333333&color=666666" alt="total GitHub release downloads" /></a>
  <a href="https://github.com/herdrdev/herdr/stargazers"><img src="https://img.shields.io/github/stars/herdrdev/herdr?labelColor=333333&color=666666&logo=github" alt="GitHub stars" /></a>
  <a href="https://github.com/herdrdev/herdr/releases/latest"><img src="https://img.shields.io/github/v/release/herdrdev/herdr?label=release&labelColor=333333&color=666666" alt="latest stable release" /></a>
  <a href="https://formulae.brew.sh/formula/herdr"><img src="https://img.shields.io/homebrew/v/herdr?label=homebrew&labelColor=333333&color=666666" alt="Homebrew version" /></a>
  <a href="https://x.com/herdrdev"><img src="https://img.shields.io/badge/follow-%40herdrdev-000000?logo=x&logoColor=white" alt="follow @herdrdev on X" /></a>
</p>

---

https://github.com/user-attachments/assets/043ec09f-4bdd-41d5-aee0-8fda6b83e267

**the runtime your coding agents live on.**

- **detach without stopping work** — herdr keeps terminals running in a background server when you close the client or lose your SSH connection. after a server or machine restart, herdr restores the saved layout and can resume supported agent sessions; the original processes do not survive. [session state →](https://herdr.dev/docs/session-state/)
- **several machines, one window** — keep local work and saved ssh machines together, with a combined agent list and independent reconnects. [remote machines →](https://herdr.dev/docs/connecting-machines/)
- **never hunt for the stuck one** — every pane is marked working, blocked, or idle. when an agent stops and needs an answer, herdr says so.
- **agent-native** — agents drive herdr through the cli and socket api: they can spawn panes, prompt each other, and wait until another agent is genuinely blocked. [agent skill →](https://herdr.dev/docs/agent-skill/)
- **runs what you already run** — claude code, codex, kimi, opencode and pi, plus anything else that runs in a terminal. herdr doesn't wrap or replace them; it owns their terminals.
- **keyboard and mouse, both first-class** — tmux-style prefix keys *and* click, drag, split. pick per moment, not per tool.
- **plugins** — extend panes and workflows. [browse the marketplace →](https://herdr.dev/plugins/)
- **one rust binary, no electron** — runs in whatever terminal you already use.

---

## install

```bash
curl -fsSL https://herdr.dev/install.sh | sh
```

or `brew install herdr` · `mise use -g herdr` · windows: `powershell -ExecutionPolicy Bypass -c "irm https://herdr.dev/install.ps1 | iex"` · [endpoint-protected Windows](https://herdr.dev/docs/windows-beta/) · [binaries](https://github.com/herdrdev/herdr/releases)

then start it where the work lives:

```bash
herdr
```

run your agents, split panes, walk away. `ctrl+b q` detaches, `herdr` reattaches. [quick start →](https://herdr.dev/docs/quick-start/)

## docs

everything lives at [herdr.dev/docs](https://herdr.dev/docs/): [quick start](https://herdr.dev/docs/quick-start/) · [concepts](https://herdr.dev/docs/concepts/) · [supported agents](https://herdr.dev/docs/agents/) · [keyboard](https://herdr.dev/docs/keyboard/) · [configuration](https://herdr.dev/docs/configuration/) · [session state](https://herdr.dev/docs/session-state/) · [connecting machines](https://herdr.dev/docs/connecting-machines/) · [remote](https://herdr.dev/docs/persistence-remote/) · [integrations](https://herdr.dev/docs/integrations/) · [add herdr support to your agent](https://herdr.dev/docs/add-herdr-support/) · [plugins](https://herdr.dev/docs/plugins/) · [socket api](https://herdr.dev/docs/socket-api/)

## thanks

every past sponsor and backer is listed in [SPONSORS.md](./SPONSORS.md) — thank you 🐑

enterprise / partnership: hey@herdr.dev

## agent instructions

if you are an ai agent helping with this repository, read [`AGENTS.md`](./AGENTS.md) before making changes and read [`CONTRIBUTING.md`](./CONTRIBUTING.md) before opening issues or PRs.

## development

```bash
git clone https://github.com/herdrdev/herdr
cd herdr
cargo build --release

just test        # unit tests
just check       # formatting, tests, and maintenance checks
```

The test orchestrator runs five phases: nextest, maintenance, UI hot-path architecture,
integration assets, and docs contract. Its combined worker budget defaults to
`min(8, cpu_count)` and can be set with `HERDR_TEST_BUDGET` or `--test-budget`. The
limits are resolved as command line > environment > default, and every explicit value
must be a positive integer:

- phase concurrency defaults to `min(2, 5, test_budget)`; use
  `HERDR_TEST_PHASE_JOBS`, `--phase-jobs`, or the `--jobs` alias;
- maintenance workers default to `min(4, test_budget)`; use
  `HERDR_MAINTENANCE_JOBS` or `--maintenance-jobs` (the standalone
  `run_parallel_unittest.py` also accepts `--jobs`);
- nextest threads default to `max(1, test_budget-maintenance_jobs)`; use
  `HERDR_NEXTEST_JOBS` or `--nextest-jobs`.

The orchestrator passes the resolved maintenance and nextest values to its phase
processes. Explicit worker overrides are not silently capped by the combined budget.
Each run gets an isolated directory under `target/test-suite-logs/<run-id>/` with
per-phase logs and an atomically updated `manifest.json`. The manifest records
`run_id`, `status`, `manifest`, `test_budget`, `phase_jobs`, `maintenance_jobs`,
`nextest_jobs`, `failures`, and, under `phases`, each phase's `recipe`, `status`,
`exit_code`, `seconds`, and `log`. Direct `just nextest-all`, `just test-one`, and
`just ci-tests` recipes use `HERDR_NEXTEST_JOBS` and default to 4 when it is unset.

For a Linux or macOS release performance smoke comparison, run:

```bash
scripts/release_perf_smoke.sh target/release/herdr
```

The smoke runs the candidate against a stable baseline for the `hidden50` and
`visible30` scenarios in two serial rounds. Set `HERDR_PERF_BASELINE_BIN` to use a
local baseline instead of downloading the stable binary, and use
`HERDR_PERF_SAMPLE_SECONDS` or `HERDR_PERF_WARMUP_SECONDS` to change the sample and
warmup durations. Each `.local/perf-baseline/run-*` run keeps its commands, metadata,
raw samples, summary, run log, and exit code, while removing only its temporary state.
The smoke and each case isolate all user state by setting `HOME`, `USERPROFILE`,
`XDG_CONFIG_HOME`, `XDG_STATE_HOME`, `XDG_RUNTIME_DIR`, `XDG_DATA_HOME`,
`XDG_CACHE_HOME`, `APPDATA`, `LOCALAPPDATA`, `HERDR_HOME`, `CODEX_HOME`,
`KIMI_CODE_HOME`, and `TMPDIR` to a short, private `.local/p-*` runtime root linked
to the evidence run by an ownership receipt. Each case uses an exclusive subdirectory,
a short session name, and a private `TMUX_TMPDIR`; inherited herdr socket/session
variables are cleared. API, client, and tmux socket paths are checked against the
platform's byte limit before launch. Readiness output, dead-pane status, and allowlisted
private logs are retained as text before cleanup. Uncertain cleanup fails the run and
retains the runtime root; only receipt-matched, confirmed-stopped state is removed.

## license

Herdr is licensed under the [Apache License 2.0](LICENSE).
