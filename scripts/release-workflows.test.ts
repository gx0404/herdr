import { describe, expect, test } from "bun:test";
import { mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { join } from "node:path";

type WorkflowDirectory = "workflows" | "workflows-archive";
const load = (name: string, directory: WorkflowDirectory = "workflows"): any =>
  Bun.YAML.parse(readFileSync(new URL(`../.github/${directory}/${name}.yml`, import.meta.url), "utf8"));
const workflowNames = (directory: WorkflowDirectory): string[] =>
  readdirSync(new URL(`../.github/${directory}/`, import.meta.url))
    .filter((name) => /\.ya?ml$/i.test(name))
    .sort();
const preview = load("preview", "workflows-archive");
const release = load("release", "workflows-archive");
const adminGate = release.jobs["validate-release-source"].steps[0];

describe("fork workflow layout", () => {
  test("only CI and GX release remain active", () => {
    expect(workflowNames("workflows")).toEqual(["ci.yml", "gx-release.yml"]);
  });

  test("upstream and redundant workflows remain available only in the archive", () => {
    expect(workflowNames("workflows-archive")).toEqual([
      "build-artifacts-manual.yml",
      "distribution.yml",
      "label-next-release-issues.yml",
      "nix.yml",
      "pr-gate.yml",
      "preview.yml",
      "release.yml",
      "website-deploy.yml",
      "windows-arm64.yml",
    ]);
  });

  test("retained workflows target the fork default branch without removing PR checks", () => {
    const ci = load("ci");
    expect(ci.on.push.branches).toEqual(["feature/gx_herdr"]);
    expect(ci.on.pull_request.types).toEqual(["opened", "synchronize", "reopened"]);
    expect(ci.jobs["conventional-commits"].if).toBe(
      "github.event_name != 'push' || github.ref_name == github.event.repository.default_branch",
    );
    expect(load("gx-release").on.workflow_dispatch.inputs.ref.default).toBe("feature/gx_herdr");
  });
});

describe("scoped native release qualification", () => {
  const ci = load("ci");
  const job = ci.jobs.check;
  const step = (name: string): any => job.steps.find((entry: any) => entry.name === name);
  const build = step("Qualify clean native release");
  const handoff = step("Qualify macOS live handoff");
  const tools = step("Prepare Unix release smoke tools");
  const perf = step("Qualify Unix release performance");
  const upload = step("Upload native release evidence");
  const raw = step("Upload Unix raw performance evidence");

  test("qualification is same-fork validation PR only, without extra permissions or ordinary PR cost", () => {
    const gate = "github.repository == 'gx0404/herdr' && github.event_name == 'pull_request' && github.event.pull_request.head.repo.full_name == 'gx0404/herdr' && github.head_ref == 'verify/runtime-sync-p11-20261006'";
    expect(job.env.RELEASE_QUALIFICATION).toBe(`\${{ ${gate} }}`);
    expect(job["timeout-minutes"]).toBe(`\${{ ${gate} && 75 || matrix.timeout_minutes }}`);
    expect(ci.permissions).toEqual({ contents: "read" });
    expect(ci.on.pull_request_target).toBeUndefined();
    expect(job.permissions).toBeUndefined();
    expect(build.if).toBe("env.RELEASE_QUALIFICATION == 'true'");
    for (const entry of [handoff, tools, perf, upload, raw]) {
      expect(entry.if).toContain("env.RELEASE_QUALIFICATION == 'true'");
      expect(entry["continue-on-error"]).toBeUndefined();
    }
    expect(job.steps.indexOf(build)).toBeGreaterThan(job.steps.indexOf(step("Replay PowerShell terminal compatibility")));
  });

  test("native standard build rejects local overrides and records separate checkout/head/merge identities", () => {
    expect(build.shell).toBe("pwsh");
    expect(build.env.PR_HEAD_SHA).toBe("${{ github.event.pull_request.head.sha }}");
    expect(build.env.PR_MERGE_SHA).toBe("${{ github.event.pull_request.merge_commit_sha }}");
    for (const token of ["git rev-parse HEAD", "checkout_sha = $checkout", "head_sha = $env:PR_HEAD_SHA", "merge_sha = $env:PR_MERGE_SHA", ".cargo/config.local.toml", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC_WRAPPER", "CARGO_PROFILE_RELEASE_.*", "Non-repository Cargo configuration is forbidden", "$env:CARGO_TERM_VERBOSE = 'true'", "just build 2>&1 | Tee-Object", "$buildExit = $LASTEXITCODE", "exit $buildExit", "target/release/herdr.exe", "target/release/conpty", "target/release/THIRD-PARTY-NOTICES", "target/release/herdr", "Get-FileHash", "artifacts.json"]) {
      expect(build.run).toContain(token);
    }
    expect(build.run).not.toContain("cargo build");
    expect(build.run).not.toContain("${{");
  });

  test.skipIf(process.platform !== "win32")("PowerShell build guard accepts controlled incremental zero or unset and rejects other overrides", () => {
    const guard = build.run.slice(build.run.indexOf("$overrides ="), build.run.indexOf("$cargoHome ="));
    expect(guard).toContain("CARGO_INCREMENTAL must be unset or exactly 0");
    expect(build.run).toContain("$receipt.cargo_incremental = $env:CARGO_INCREMENTAL");
    expect(build.run).toContain("environment (pinned dtolnay/rust-toolchain sets 0)");
    const cleanEnv = Object.fromEntries(Object.entries(process.env).filter(([key]) => !/^(RUST|CARGO_)/i.test(key)));
    for (const [incremental, rustflags, error] of [
      [undefined, undefined, undefined],
      ["0", undefined, undefined],
      ["1", undefined, "CARGO_INCREMENTAL must be unset or exactly 0"],
      ["false", undefined, "CARGO_INCREMENTAL must be unset or exactly 0"],
      ["00", undefined, "CARGO_INCREMENTAL must be unset or exactly 0"],
      [" 0", undefined, "CARGO_INCREMENTAL must be unset or exactly 0"],
      ["0", "-C opt-level=0", "Build overrides are forbidden: RUSTFLAGS"],
    ] as const) {
      const result = spawnSync("pwsh", ["-NoProfile", "-NonInteractive", "-Command", `$ErrorActionPreference = 'Stop'\n${guard}\nWrite-Output 'guard accepted'`], {
        env: { ...cleanEnv, ...(incremental === undefined ? {} : { CARGO_INCREMENTAL: incremental }), ...(rustflags === undefined ? {} : { RUSTFLAGS: rustflags }) },
        encoding: "utf8",
      });
      expect({ incremental, rustflags, status: result.status }).toEqual({ incremental, rustflags, status: error ? 1 : 0 });
      if (error) {
        expect(result.stderr).toContain(error);
        expect(result.stdout).not.toContain("guard accepted");
      } else {
        expect(result.stderr).toBe("");
        expect(result.stdout.trim()).toBe("guard accepted");
      }
    }
  }, 30000);

  test.skipIf(process.platform !== "win32")("PowerShell toolchain loop preserves complete arguments for every command", () => {
    const loop = build.run.slice(build.run.indexOf("foreach ($spec in"), build.run.indexOf("$env:CARGO_TERM_VERBOSE ="));
    const result = spawnSync("pwsh", ["-NoProfile", "-NonInteractive", "-Command", `
$ErrorActionPreference = 'Stop'
$out = 'unused-stub-output'
foreach ($name in @('rustc', 'cargo', 'just', 'bun', 'zig')) {
  Set-Item "Function:$name" {
    [ordered]@{ command = $MyInvocation.MyCommand.Name; arguments = @($args) } | ConvertTo-Json -Compress
    $global:LASTEXITCODE = 0
  }
}
function Tee-Object {
  param([Parameter(ValueFromPipeline)]$InputObject, [string]$FilePath, [switch]$Append)
  process { $InputObject }
}
${loop}
`], { encoding: "utf8" });
    expect({ status: result.status, stderr: result.stderr }).toEqual({ status: 0, stderr: "" });
    expect(result.stdout.trim().split(/\r?\n/).map((line) => JSON.parse(line))).toEqual([
      { command: "rustc", arguments: ["-vV"] },
      { command: "cargo", arguments: ["-V"] },
      { command: "just", arguments: ["--version"] },
      { command: "bun", arguments: ["--version"] },
      { command: "zig", arguments: ["version"] },
      { command: "cargo", arguments: ["nextest", "--version"] },
    ]);
  }, 30000);

  test("macOS missing tests run explicitly before unchanged serial 60s smoke", () => {
    expect(handoff.if).toContain("runner.os == 'macOS'");
    expect(handoff.run).toContain("cargo nextest run --locked --no-fail-fast -E 'binary(live_handoff)'");
    expect(handoff["continue-on-error"]).toBeUndefined();
    expect(handoff.run).toContain('if (( codes[0] != 0 )); then exit "${codes[0]}"; fi');
    expect(job.steps.indexOf(handoff)).toBeLessThan(job.steps.indexOf(perf));
    expect(perf.if).toContain("matrix.kind == 'unix'");
    expect(perf.env).toEqual({ HERDR_PERF_SAMPLE_SECONDS: "60" });
    expect(perf.run).toContain("just bench-release-smoke 2>&1 | tee");
    expect(perf.run).toContain("Baseline override is forbidden");
    expect(perf.run).toContain("distribution/latest.json");
    expect(perf.run).toContain("manifest_sha256");
    expect(perf.run).toContain("baseline.stat().st_size");
    expect(perf.run).toContain("hashlib.sha256(baseline.read_bytes())");
    for (const entry of [handoff, perf]) {
      expect(entry.run).toContain("set +e\n");
      expect(entry.run).toContain('codes=("${PIPESTATUS[@]}")');
      expect(entry.run).toContain('exit "${codes[0]}"');
      expect(entry.run).toContain('exit "${codes[1]}"');
    }
    expect(tools.run).toContain('"$RUNNER_ENVIRONMENT" == github-hosted');
    expect(tools.run).toContain("jq lsof perl tee tmux curl");
    expect(tools.run).toContain("command -v pidstat");
    expect(tools.run).toContain('sudo apt-get install -y --no-install-recommends "${packages[@]}"');
    expect(tools.run).toContain('brew install "${packages[@]}"');
  });

  test("failure artifacts use the existing pin and bounded paths, never private runtime or baseline executables", () => {
    for (const entry of [upload, raw]) {
      expect(entry.if).toStartWith("always() && env.RELEASE_QUALIFICATION == 'true'");
      expect(entry.uses).toBe("actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a");
      expect(entry.with["retention-days"]).toBe(14);
    }
    expect(upload.with.path).toBe("target/ci-release-evidence/");
    expect(upload.with["include-hidden-files"]).toBeUndefined();
    expect(raw.if).toContain("steps.release-perf.outputs.run_dir != ''");
    expect(raw.with["include-hidden-files"]).toBe(true);
    expect(raw.with.path.trim().split("\n")).toEqual([
      "${{ steps.release-perf.outputs.run_dir }}/*.txt",
      "${{ steps.release-perf.outputs.run_dir }}/run.log",
      "${{ steps.release-perf.outputs.run_dir }}/baseline-receipt.json",
      "${{ steps.release-perf.outputs.run_dir }}/results/**/*.txt",
    ]);
  });

  test("qualification bash steps parse without running tools, builds or performance probes", () => {
    for (const entry of [handoff, tools, perf]) {
      const result = spawnSync("bash", ["-n"], { input: entry.run, encoding: "utf8" });
      expect({ name: entry.name, status: result.status, stderr: result.stderr }).toEqual({ name: entry.name, status: 0, stderr: "" });
    }
  });

  test.skipIf(process.platform !== "win32")("native release PowerShell parses without running a build", () => {
    const result = spawnSync("pwsh", ["-NoProfile", "-NonInteractive", "-Command",
      "$tokens = $null; $errors = $null; [void][System.Management.Automation.Language.Parser]::ParseInput($env:GX_SCRIPT, [ref]$tokens, [ref]$errors); if ($errors.Count) { $errors | Out-String | Write-Error; exit 1 }",
    ], { env: { ...process.env, GX_SCRIPT: build.run }, encoding: "utf8" });
    expect({ status: result.status, stderr: result.stderr }).toEqual({ status: 0, stderr: "" });
  }, 30000);
});

describe("archived official publishing workflow boundaries", () => {
  test("publishing is tag-only while normal PR CI remains enabled", () => {
    expect(preview.on).toEqual({ push: { tags: ["preview-*"] } });
    expect(release.on).toEqual({ push: { tags: ["v*"] } });
    expect(load("ci").on.pull_request).toBeDefined();
  });

  test("preview checks do not require a workstation Windows SDK", () => {
    const checks = preview.jobs.preflight.steps.find((step: any) => step.name === "Run checks");
    expect(checks.run.trim().split("\n")).toEqual(["just ci", "just docs-contract-test"]);
    expect(preview.jobs.build.strategy.matrix.include).toContainEqual({
      target: "x86_64-pc-windows-msvc",
      os: "windows-latest",
      name: "herdr-windows-x86_64.zip",
    });
    expect(preview.jobs.publish.needs).toContain("build");
  });

  test("each publishing job rechecks both actors before using credentials", () => {
    for (const [workflow, names] of [
      [preview, ["preflight", "publish"]],
      [release, ["validate-release-source", "release", "update-nix-package", "close-released-issues", "update-latest-json"]],
    ] as const) {
      for (const name of names) {
        const job = workflow.jobs[name];
        expect(job.if).toContain("github.event_name == 'push'");
        expect(job.if).toContain("startsWith(github.ref, 'refs/tags/");
        expect(job.steps[0]).toEqual(adminGate);
      }
    }
    expect(adminGate.run).toContain('"$GITHUB_ACTOR" "$GITHUB_TRIGGERING_ACTOR"');
    expect(adminGate.env.GH_TOKEN).toBe("${{ github.token }}");
    expect(adminGate.run).not.toContain("ogulcancelik");
  });

  test("release arguments are not interpolated into executable shell text", () => {
    const input = `untrusted'\"$(echo unexpected-command)`;
    for (const args of [
      ["preview", input],
      ["release-prepare", input, input],
      ["release-publish", input, input],
      ["release", input, input],
    ]) {
      const result = spawnSync("just", ["--dry-run", ...args], { encoding: "utf8" });
      expect(result.status).toBe(0);
      expect(result.stdout + result.stderr).not.toContain(input);
      expect(result.stdout + result.stderr).not.toContain("unexpected-command");
    }
  });

  test.skipIf(process.platform === "win32")("admin gate permits admins and fails closed for other roles or API errors", () => {
    const dir = mkdtempSync("/var/tmp/herdr-admin-gate-");
    try {
      writeFileSync(join(dir, "gh"), `#!/bin/sh
case "$2" in
  */collaborators/admin-*/permission) echo admin ;;
  */collaborators/maintainer/permission) echo maintain ;;
  */collaborators/writer/permission) echo write ;;
  *) exit 1 ;;
esac
`, { mode: 0o755 });
      for (const [actor, trigger, succeeds] of [
        ["admin-one", "admin-two", true],
        ["writer", "admin-two", false],
        ["admin-one", "writer", false],
        ["admin-one", "maintainer", false],
        ["admin-one", "api-error", false],
      ] as const) {
        const result = spawnSync("bash", ["-c", adminGate.run], {
          env: { ...process.env, PATH: `${dir}:${process.env.PATH}`, GITHUB_REPOSITORY: "example/test", GITHUB_ACTOR: actor, GITHUB_TRIGGERING_ACTOR: trigger },
          encoding: "utf8",
        });
        expect(result.status === 0).toBe(succeeds);
      }
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });
});

const gx = load("gx-release");
const runs = (job: any): string => job.steps.map((step: any) => step.run || "").join("\n");

describe("fork GX installer release boundaries", () => {
  test("dispatch is fork-only, build-only by default and never cancels an active release", () => {
    expect(Object.keys(gx.on)).toEqual(["workflow_dispatch"]);
    expect(gx.on.workflow_dispatch.inputs.ref.type).toBe("string");
    expect(gx.on.workflow_dispatch.inputs.ref.required).toBe(true);
    expect(gx.on.workflow_dispatch.inputs.publish).toMatchObject({ type: "boolean", default: false });
    expect(gx.on.workflow_dispatch.inputs.version).toBeUndefined();
    expect(gx.jobs.prepare.if).toContain("github.repository == 'gx0404/herdr'");
    expect(gx.jobs.prepare.if).toContain("github.event_name == 'workflow_dispatch'");
    expect(gx.concurrency).toEqual({ group: "gx-release", "cancel-in-progress": false });
  });

  test("only publishing receives write permission and every checkout discards credentials", () => {
    expect(gx.permissions).toEqual({ contents: "read" });
    for (const [name, job] of Object.entries<any>(gx.jobs)) {
      expect(job.permissions || gx.permissions).toEqual({ contents: name === "publish" ? "write" : "read" });
      const checkout = job.steps.find((step: any) => step.uses?.startsWith("actions/checkout@"));
      expect(checkout.with["persist-credentials"]).toBe(false);
      expect(checkout.with.ref).toBe(name === "prepare" ? "${{ inputs.ref }}" : "${{ needs.prepare.outputs.sha }}");
    }
    expect(gx.jobs.publish.if).toContain("inputs.publish");
    expect(gx.jobs.publish.if).toContain("github.repository == 'gx0404/herdr'");
    expect(gx.jobs.publish.if).toContain("github.event_name == 'workflow_dispatch'");
  });

  test("inputs and SHA travel through environment rather than executable shell interpolation", () => {
    for (const job of Object.values<any>(gx.jobs)) {
      for (const step of job.steps) {
        expect(step.run || "").not.toContain("${{");
      }
    }
    const prepare = gx.jobs.prepare.steps.find((step: any) => step.id === "prepare");
    expect(prepare.env.GX_PUBLISH_REQUESTED).toBe("${{ inputs.publish }}");
    expect(prepare.run).toContain("scripts/gx_release.py prepare");
    const publish = gx.jobs.publish.steps.at(-1);
    expect(publish.env.GX_SHA).toBe("${{ needs.prepare.outputs.sha }}");
    expect(publish.env.GX_VERSION).toBe("${{ needs.prepare.outputs.version }}");
    expect(publish.run).toContain('--repository gx0404/herdr --version "$GX_VERSION" --sha "$GX_SHA"');
    expect(publish.run).not.toContain("--clobber");
  });

  test("toolchain outputs are checked against the Rust and Zig sources and actions are SHA pinned", () => {
    const rust = Bun.TOML.parse(readFileSync(new URL("../rust-toolchain.toml", import.meta.url), "utf8")) as any;
    expect(rust.toolchain.channel).toBe("1.96.1");
    const zig = readFileSync(new URL("setup_zig.py", import.meta.url), "utf8");
    expect(zig).toContain('ZIG_VERSION = "0.16.0"');
    for (const job of Object.values<any>(gx.jobs)) {
      for (const step of job.steps) {
        if (step.uses) expect(step.uses).toMatch(/@[0-9a-f]{40}$/);
        if (step.uses?.startsWith("dtolnay/rust-toolchain@")) {
          expect(step.with.toolchain).toBe("${{ needs.prepare.outputs.rust }}");
        }
        if (step.uses?.startsWith("vercel-labs/setup-zig@")) {
          expect(step.with.version).toBe("${{ needs.prepare.outputs.zig }}");
        }
      }
    }
    const inno = runs(gx.jobs.windows);
    expect(inno).toContain("innosetup-7.1.0-x64.exe");
    expect(inno).toContain("0362a383ed217d4c4239b5933866dd96d3eb2102737da92f80f6057a4b40df2f");
    expect(inno).toContain("Inno Setup checksum mismatch");
    expect(inno).toContain("Unexpected Inno Setup version");
    expect(inno).toContain("(& $compiler --version | Out-String).Trim()");
    expect(inno).toContain("$LASTEXITCODE -ne 0 -or $version -ne '7.1.0'");
    expect(inno).not.toContain("VersionInfo.ProductVersion");
    expect(inno).toContain("RUNNER_ENVIRONMENT");
  });

  test("repository checks gate parallel builders and both builders use the shared package CLI", () => {
    expect(runs(gx.jobs.checks)).toContain("just ci\njust docs-contract-test");
    expect(runs(gx.jobs.checks)).toContain("scripts/resolve_agent_rules.py --check");
    expect(runs(gx.jobs.checks)).toContain("python3-tomli");
    for (const platform of ["windows", "linux"]) {
      expect(gx.jobs[platform].needs).toEqual(platform === "windows" ? ["prepare", "checks", "previous"] : ["prepare", "checks"]);
      expect(runs(gx.jobs[platform])).toContain(`scripts/gx_package.py --platform ${platform} --output-dir target/gx-${platform}`);
      expect(runs(gx.jobs[platform])).not.toContain("--allow-dirty");
    }
    expect(runs(gx.jobs.linux)).toContain("musl-tools");
    expect(runs(gx.jobs.windows)).toContain("scripts.test_gx_package scripts.test_gx_release scripts.test_package_windows_conpty");
  });

  test("previous release resolution is unconditional, read-only and isolated from candidate artifacts", () => {
    expect(gx.on.workflow_dispatch.inputs.previous_tag).toMatchObject({ type: "string", required: false, default: "" });
    const previous = gx.jobs.previous;
    expect(previous.if).toBeUndefined();
    expect(previous.needs).toBe("prepare");
    expect(previous.permissions || gx.permissions).toEqual({ contents: "read" });
    const resolve = previous.steps.find((step: any) => step.id === "previous");
    expect(resolve.env.GX_PREVIOUS_TAG).toBe("${{ inputs.previous_tag }}");
    expect(resolve.run).toContain('previous --repository gx0404/herdr --previous-tag "$GX_PREVIOUS_TAG"');
    expect(resolve.run).not.toContain("||");
    expect(runs(previous)).toContain("python3 -m unittest scripts.test_gx_release");
    expect(previous.outputs.sha).toBe("${{ steps.previous.outputs.sha }}");
    expect(previous.outputs.tag).toBe("${{ steps.previous.outputs.tag }}");
    const upload = previous.steps.find((step: any) => step.uses?.startsWith("actions/upload-artifact@"));
    expect(upload.with.name).toBe("previous-packages");
    expect(upload.with.name).not.toMatch(/^gx-/);
    expect(upload.if).toBe("steps.previous.outputs.available == 'true'");
    expect(gx.jobs.verify.steps.find((step: any) => step.uses?.startsWith("actions/download-artifact@")).with.pattern).toBe("gx-*");
  });

  test("Windows and both disposable Ubuntu containers consume verified previous packages for real upgrades", () => {
    const windows = gx.jobs.windows.steps.find((step: any) => step.run?.includes("gx_smoke_windows.ps1"));
    expect(windows.run).toContain("gx_smoke_windows.ps1 -InstallerPath");
    expect(windows.run).toContain("-ExpectedVersion $env:GX_VERSION @previous");
    expect(windows.run).toContain("$previous.PreviousInstallerPath");
    expect(windows.env.GX_PREVIOUS_EXE).toBe("${{ needs.previous.outputs.windows }}");
    expect(windows.env.HERDR_GX_DISPOSABLE).toBe("1");
    const smoke = gx.jobs["linux-smoke"];
    expect(smoke.needs).toEqual(["prepare", "linux", "previous"]);
    expect(smoke.strategy.matrix.ubuntu).toEqual(["22.04", "24.04"]);
    expect(smoke.steps.find((step: any) => step.uses?.startsWith("actions/download-artifact@")).with.name).toBe("gx-linux-deb");
    for (const job of [gx.jobs.windows, smoke]) {
      const download = job.steps.find((step: any) => step.with?.name === "previous-packages");
      expect(download.if).toBe("needs.previous.outputs.available == 'true'");
      expect(download.with.path).toBe("target/gx-previous");
      const step = job.steps.find((step: any) => step.env?.GX_PREVIOUS_TAG);
      expect(step.env.GX_PREVIOUS_TAG).toBe("${{ needs.previous.outputs.tag }}");
      expect(step.env.GX_PREVIOUS_SHA).toBe("${{ needs.previous.outputs.sha }}");
    }
    expect(runs(smoke)).toContain('docker run --rm --init --volume "$PWD:/workspace:ro"');
    expect(runs(smoke)).toContain('"ubuntu:$GX_UBUNTU" bash -eu -c');
    expect(runs(smoke)).toContain("python3 bash");
    expect(runs(smoke)).toContain("util-linux passwd coreutils");
    expect(runs(smoke)).toContain("--env GX_PREVIOUS_DEB --env HERDR_GX_DISPOSABLE=1");
    expect(runs(smoke)).toContain('previous+=("/workspace/target/gx-previous/$GX_PREVIOUS_DEB")');
    expect(runs(smoke)).toContain('bash scripts/gx_smoke_linux.sh "/workspace/target/gx-linux/herdr-gx_${GX_VERSION}_amd64.deb" "${previous[@]}"');
    expect(runs(smoke)).toContain("upgrade PASS");
    expect(runs(smoke)).toContain("old-to-new upgrade: N/A (no older published GX release exists)");
    expect(runs(smoke)).not.toContain("sudo apt install");
  });

  test("all GX bash steps parse without executing builds, containers or publication", () => {
    for (const [name, job] of Object.entries<any>(gx.jobs)) {
      for (const step of job.steps) {
        if (!step.run || step.shell === "pwsh") continue;
        const result = spawnSync("bash", ["-n"], { input: step.run, encoding: "utf8" });
        expect({ job: name, stderr: result.stderr, status: result.status }).toEqual({ job: name, stderr: "", status: 0 });
      }
    }
  });

  test.skipIf(process.platform !== "win32")("GX PowerShell steps parse without installing anything", () => {
    for (const step of gx.jobs.windows.steps) {
      if (!step.run) continue;
      const result = spawnSync("pwsh", ["-NoProfile", "-NonInteractive", "-Command",
        "$tokens = $null; $errors = $null; [void][System.Management.Automation.Language.Parser]::ParseInput($env:GX_SCRIPT, [ref]$tokens, [ref]$errors); if ($errors.Count) { $errors | Out-String | Write-Error; exit 1 }",
      ], { env: { ...process.env, GX_SCRIPT: step.run }, encoding: "utf8" });
      expect({ stderr: result.stderr, status: result.status }).toEqual({ stderr: "", status: 0 });
    }
  }, 30000);

  test("unconditional unified verification gates publication including build-only runs", () => {
    expect(gx.jobs.verify.if).toBeUndefined();
    expect(gx.jobs.verify.needs).toEqual(["prepare", "checks", "previous", "windows", "linux", "linux-smoke"]);
    expect(runs(gx.jobs.verify)).toContain("python3 -m unittest scripts.test_gx_release");
    expect(runs(gx.jobs.verify)).toContain('scripts/gx_release.py verify --version "$GX_VERSION" --sha "$GX_SHA"');
    expect(gx.jobs.publish.needs).toEqual(["prepare", "verify"]);
    expect(gx.jobs.publish.steps.find((step: any) => step.uses?.startsWith("actions/download-artifact@")).with.name).toBe("verified-release");
    for (const job of Object.values<any>(gx.jobs)) {
      expect(runs(job)).not.toContain("distribution/latest.json");
      expect(runs(job)).not.toContain("distribution/preview.json");
      expect(runs(job)).not.toContain("git push");
    }
  });
});
