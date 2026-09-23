import { afterEach, expect, test } from "bun:test";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { createServer, type Server } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";

const script = join(import.meta.dir, "herdr-agent-state.sh");

let server: Server | undefined;
let tempDir: string | undefined;

afterEach(async () => {
  const current = server;
  server = undefined;
  if (current) {
    await new Promise<void>((resolve) => current.close(() => resolve()));
  }
  if (tempDir) {
    await rm(tempDir, { recursive: true, force: true });
    tempDir = undefined;
  }
});

type Request = { id: string; method: string; params: Record<string, unknown> };

async function listen(): Promise<{ socketPath: string; requests: Request[] }> {
  tempDir = await mkdtemp(join(tmpdir(), "herdr-kimi-hook-"));
  const socketPath = join(tempDir, "herdr.sock");
  const requests: Request[] = [];
  const listening = createServer((socket) => {
    let buffer = "";
    socket.on("data", (chunk) => {
      buffer += chunk.toString();
      const newline = buffer.indexOf("\n");
      if (newline < 0) return;
      requests.push(JSON.parse(buffer.slice(0, newline)));
      socket.end('{"id":"ok","result":{"type":"ok"}}\n');
    });
  });
  server = listening;
  await new Promise<void>((resolve) => listening.listen(socketPath, resolve));
  return { socketPath, requests };
}

// The hook must never see the caller's real HERDR_* variables: a test run inside
// a Herdr pane would otherwise report into the live session.
async function runHook(action: string, stdin: string, env: Record<string, string>) {
  const proc = Bun.spawn(["sh", script, action], {
    stdin: new Blob([stdin]),
    stdout: "ignore",
    stderr: "ignore",
    env: { PATH: process.env.PATH ?? "/usr/bin:/bin", ...env },
  });
  return await proc.exited;
}

function herdrEnv(socketPath: string) {
  return { HERDR_ENV: "1", HERDR_SOCKET_PATH: socketPath, HERDR_PANE_ID: "test:p1" };
}

// Kimi validates the whole `config.toml` against a fixed hook-event enum, so an
// event name that the minimum supported Kimi does not know invalidates the file
// and takes the lifecycle hooks down with it. This is the enum of the minimum
// version itself: `HOOK_EVENT_TYPES` in packages/agent-core/src/session/hooks/
// types.ts at the official MoonshotAI/kimi-code tag `@moonshot-ai/kimi-code@0.14.0`,
// which `HookDefSchema` validates `[[hooks]]` against. Kimi Code 2.0.2 keeps the
// same 16 names in node-sdk and adds UserPromptQueued, TurnStarted,
// SessionHeartbeat and TaskStarted (0.32.0) only in agent-core-v2.
const HOOK_EVENTS_AT_MIN_VERSION = new Set([
  "PreToolUse",
  "PostToolUse",
  "PostToolUseFailure",
  "PermissionRequest",
  "PermissionResult",
  "UserPromptSubmit",
  "Stop",
  "StopFailure",
  "Interrupt",
  "SessionStart",
  "SessionEnd",
  "SubagentStart",
  "SubagentStop",
  "PreCompact",
  "PostCompact",
  "Notification",
]);

test("every installed hook event exists at the minimum supported Kimi version", async () => {
  const source = await readFile(join(import.meta.dir, "..", "..", "mod.rs"), "utf8");

  // Raising the floor is exactly when the allowed set above has to be revisited.
  expect(source).toContain('const KIMI_MIN_VERSION: &str = "0.14.0";');

  const table = source.match(/const KIMI_HOOK_EVENTS: \[\(&str, Option<&str>, &str\); (\d+)\] = \[([\s\S]*?)\n\];/);
  expect(table).not.toBeNull();
  const [, declaredLength, body] = table!;
  const events = [...body.matchAll(/\(\s*"([A-Za-z]+)"/g)].map((match) => match[1]);

  expect(events).toHaveLength(Number(declaredLength));
  expect(events.filter((event) => !HOOK_EVENTS_AT_MIN_VERSION.has(event))).toEqual([]);
});

test("a task_id payload maps to a background task node whatever the event is", async () => {
  const { socketPath, requests } = await listen();
  // TaskStarted is not installed at the current KIMI_MIN_VERSION; the script
  // already handles it so the subscription can come back as a one-line change.
  const payload = {
    hook_event_name: "TaskStarted",
    session_id: "session_0f0f0f0f-1111-4222-8333-444455556666",
    task_id: "bash-abc12345",
    kind: "process",
    description: "cargo test",
  };

  expect(await runHook("activity", JSON.stringify(payload), herdrEnv(socketPath))).toBe(0);

  expect(requests).toHaveLength(1);
  const [request] = requests;
  expect(request.method).toBe("pane.report_agent_activity");
  expect(request.params).toEqual({
    pane_id: "test:p1",
    source: "herdr:kimi",
    agent: "kimi",
    seq: expect.any(Number),
    hint: "TaskStarted",
    node_id: "task:bash-abc12345",
  });
});

test("task notifications name the task node, agent-kind tasks send a bare hint", async () => {
  const { socketPath, requests } = await listen();
  const notification = {
    hook_event_name: "Notification",
    notification_type: "task.completed",
    source_kind: "background_task",
    agent_id: "main",
  };

  await runHook("activity", JSON.stringify({ ...notification, source_id: "bash-zz990011" }), herdrEnv(socketPath));
  await runHook("activity", JSON.stringify({ ...notification, source_id: "agent-k2k2k2k2" }), herdrEnv(socketPath));

  expect(requests.map((request) => request.params.node_id)).toEqual(["task:bash-zz990011", undefined]);
  expect(requests.map((request) => request.params.hint)).toEqual(["Notification", "Notification"]);
});

test("subagent events and unreadable payloads still send a hint", async () => {
  const { socketPath, requests } = await listen();

  await runHook("activity", JSON.stringify({ hook_event_name: "SubagentStop", agent_name: "coder" }), herdrEnv(socketPath));
  await runHook("activity", "not json", herdrEnv(socketPath));
  await runHook(
    "activity",
    JSON.stringify({ hook_event_name: "bad name; rm -rf /", task_id: "../../etc/passwd" }),
    herdrEnv(socketPath),
  );

  expect(requests).toHaveLength(3);
  for (const request of requests) {
    expect(request.method).toBe("pane.report_agent_activity");
    expect(request.params.node_id).toBeUndefined();
    expect(request.params.agent_session_id).toBeUndefined();
    expect(request.params.state).toBeUndefined();
  }
  expect(requests.map((request) => request.params.hint)).toEqual(["SubagentStop", undefined, undefined]);
});

test("lifecycle actions keep their existing request shapes", async () => {
  const { socketPath, requests } = await listen();
  const sessionId = "session_0f0f0f0f-1111-4222-8333-444455556666";

  await runHook("working", JSON.stringify({ hook_event_name: "PreToolUse", session_id: sessionId }), herdrEnv(socketPath));
  await runHook("session", JSON.stringify({ hook_event_name: "SessionStart", session_id: sessionId }), herdrEnv(socketPath));
  await runHook("session", JSON.stringify({ hook_event_name: "SessionStart" }), herdrEnv(socketPath));

  expect(requests.map((request) => request.method)).toEqual(["pane.report_agent", "pane.report_agent_session"]);
  expect(requests[0].params).toMatchObject({ state: "working", agent_session_id: sessionId });
  expect(requests[1].params).toMatchObject({ session_start_source: "startup", agent_session_id: sessionId });
});

test("a turn that ends with an error reports idle, like a completed turn", async () => {
  const { socketPath, requests } = await listen();
  const sessionId = "session_0f0f0f0f-1111-4222-8333-444455556666";
  // Kimi fires StopFailure instead of Stop when a turn fails (smoke M3: a
  // provider 401). The payload is the camelCase-to-snake_case hook input.
  const payload = {
    hook_event_name: "StopFailure",
    session_id: sessionId,
    cwd: "/tmp/project",
    error_type: "APIStatusError",
    error_message: "401 invalid api key",
  };

  expect(await runHook("idle", JSON.stringify(payload), herdrEnv(socketPath))).toBe(0);

  expect(requests).toHaveLength(1);
  expect(requests[0].method).toBe("pane.report_agent");
  expect(requests[0].params).toEqual({
    pane_id: "test:p1",
    source: "herdr:kimi",
    agent: "kimi",
    seq: expect.any(Number),
    state: "idle",
    agent_session_id: sessionId,
  });
});

test("the installed hook table maps every turn-ending event to idle", async () => {
  const source = await readFile(join(import.meta.dir, "..", "..", "mod.rs"), "utf8");
  const table = source.match(/const KIMI_HOOK_EVENTS: \[\(&str, Option<&str>, &str\); \d+\] = \[([\s\S]*?)\n\];/);
  expect(table).not.toBeNull();
  const idleEvents = [...table![1].matchAll(/\(\s*"([A-Za-z]+)",\s*None,\s*"idle"\s*\)/g)].map((match) => match[1]);

  expect(idleEvents.sort()).toEqual(["Interrupt", "Stop", "StopFailure"]);
});

test("the hook stays silent outside Herdr and for unknown actions", async () => {
  const { socketPath, requests } = await listen();
  const payload = JSON.stringify({ hook_event_name: "TaskStarted", task_id: "bash-abc12345" });

  await runHook("activity", payload, { HERDR_SOCKET_PATH: socketPath, HERDR_PANE_ID: "test:p1" });
  await runHook("teleport", payload, herdrEnv(socketPath));

  expect(requests).toHaveLength(0);
});

test("the POSIX and PowerShell assets declare the same integration version", async () => {
  const marker = /HERDR_INTEGRATION_VERSION=(\d+)/;
  const posix = (await readFile(script, "utf8")).match(marker)?.[1];
  const powershell = (await readFile(join(import.meta.dir, "herdr-agent-state.ps1"), "utf8")).match(marker)?.[1];
  expect(posix).toBeDefined();
  expect(powershell).toBe(posix);
});
