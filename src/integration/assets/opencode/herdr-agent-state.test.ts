import { afterEach, beforeEach, expect, mock, test } from "bun:test";

const originalArgv = process.argv;
afterEach(() => { process.argv = originalArgv; });

const requests: unknown[] = [];
const clients: FakeClient[] = [];
const requestWaiters: Array<() => void> = [];
let autoAcknowledge = true;
let importCounter = 0;

type FakeClient = {
  emit: (event: string) => void;
};

mock.module("node:net", () => ({
  default: {
    createConnection(_path: string, onConnect: () => void) {
      const handlers = new Map<string, () => void>();
      const client = {
        write(input: string) {
          requests.push(JSON.parse(input.trim()));
          requestWaiters.shift()?.();
          if (autoAcknowledge) {
            queueMicrotask(() => client.emit("data"));
          }
        },
        setTimeout() {},
        on(event: string, handler: () => void) {
          handlers.set(event, handler);
        },
        destroy() {},
        emit(event: string) {
          handlers.get(event)?.();
        },
      };
      clients.push(client);
      queueMicrotask(onConnect);
      return client;
    },
  },
}));

beforeEach(() => {
  requests.length = 0;
  clients.length = 0;
  requestWaiters.length = 0;
  autoAcknowledge = true;
  process.argv = ["bun", "/$bunfs/root/src/index.js", "run"];
  process.env.HERDR_ENV = "1";
  process.env.HERDR_SOCKET_PATH = "test.sock";
  process.env.HERDR_PANE_ID = "test:p1";
});

async function loadPlugin() {
  importCounter += 1;
  const { HerdrAgentStatePlugin } = await import(`./herdr-agent-state.js?test=${importCounter}`);
  return HerdrAgentStatePlugin();
}

function waitForNextRequest(): Promise<void> {
  return new Promise((resolve) => requestWaiters.push(resolve));
}

// Lifecycle reports only; activity hints are asserted separately.
function lifecycle(): unknown[] {
  return requests.filter((request) => requestMethod(request) !== "pane.report_agent_activity");
}

function activityHints(): unknown[] {
  return requests
    .filter((request) => requestMethod(request) === "pane.report_agent_activity")
    .map((request) => requestParam(request, "hint"));
}

test("serializes lifecycle reports", async () => {
  autoAcknowledge = false;
  const plugin = await loadPlugin();
  // Each session event first queues its activity hint, then its state report.
  const firstHintDispatched = waitForNextRequest();
  const working = plugin.event({
    event: {
      type: "session.status",
      properties: { sessionID: "root-session", status: { type: "busy" } },
    },
  });
  await firstHintDispatched;
  const firstDispatched = waitForNextRequest();
  clients[0]?.emit("data");
  await firstDispatched;

  const idle = plugin.event({
    event: {
      type: "session.status",
      properties: { sessionID: "root-session", status: { type: "idle" } },
    },
  });
  expect(clients).toHaveLength(2);

  const secondHintDispatched = waitForNextRequest();
  clients[1]?.emit("data");
  await secondHintDispatched;
  expect(clients).toHaveLength(3);
  const secondDispatched = waitForNextRequest();
  clients[2]?.emit("data");
  await secondDispatched;
  expect(clients).toHaveLength(4);
  clients[3]?.emit("data");
  await Promise.all([working, idle]);

  expect(lifecycle().map(requestState)).toEqual(["working", "idle"]);
  const sequences = lifecycle().map(requestSeq);
  expect(sequences[0]).toEqual(expect.any(Number));
  expect(sequences[1]).toBe((sequences[0] as number) + 1);
  expect(activityHints()).toEqual(["session.status", "session.status"]);
});

// Smoke M4. OpenCode delivers events without awaiting plugin handlers, and a
// turn ends in one burst: the last loop step publishes busy, then the runner
// publishes idle and session.idle. The pane must end idle, in event order.
test("a turn that ends with a busy/idle burst leaves the pane idle", async () => {
  const plugin = await loadPlugin();

  await Promise.all([
    plugin.event({
      event: {
        type: "session.status",
        properties: { sessionID: "root-session", status: { type: "busy" } },
      },
    }),
    plugin.event({
      event: {
        type: "session.status",
        properties: { sessionID: "root-session", status: { type: "idle" } },
      },
    }),
    plugin.event({
      event: { type: "session.idle", properties: { sessionID: "root-session" } },
    }),
  ]);

  expect(lifecycle().map(requestState)).toEqual(["working", "idle", "idle"]);
  const sequences = lifecycle().map(requestSeq) as number[];
  expect(sequences[1]).toBeGreaterThan(sequences[0]);
  expect(sequences[2]).toBeGreaterThan(sequences[1]);
});

// Smoke M4. A plugin can store a message without starting a turn
// (`session.prompt` with `noReply: true`, e.g. oh-my-openagent's silent
// background-task wake). OpenCode still runs the `chat.message` hook and
// publishes message events for it, but no busy/idle status follows, so nothing
// about that message may put an idle pane back to working.
test("a message stored without starting a turn keeps the pane idle", async () => {
  const plugin = await loadPlugin();
  const status = (type: string) =>
    plugin.event({
      event: { type: "session.status", properties: { sessionID: "root-session", status: { type } } },
    });

  await status("busy");
  await status("idle");
  await plugin.event({ event: { type: "session.idle", properties: { sessionID: "root-session" } } });

  const message = { id: "msg-injected", sessionID: "root-session", role: "user" };
  const parts = [{ id: "prt-injected", type: "text", text: "background task finished" }];
  await plugin["chat.message"]?.({ sessionID: "root-session" }, { message, parts });
  await plugin.event({ event: { type: "message.updated", properties: { info: message } } });
  await plugin.event({
    event: { type: "message.part.updated", properties: { part: { ...parts[0], sessionID: "root-session" } } },
  });

  expect(lifecycle().map(requestState)).toEqual(["working", "idle", "idle"]);
});

// OpenCode calls every plugin's event hook in one loop without awaiting it, so
// a hook that throws synchronously would keep the event from later plugins.
test("the event hook reports failures asynchronously, never by throwing", async () => {
  const plugin = await loadPlugin();
  const broken = new Proxy(
    {},
    {
      get() {
        throw new Error("unreadable event");
      },
    },
  );

  let result: Promise<unknown> | undefined;
  expect(() => {
    result = plugin.event({ event: broken });
  }).not.toThrow();
  await expect(result).rejects.toThrow("unreadable event");
});

test("a parent cycle in session data cannot hang the event handler", async () => {
  const plugin = await loadPlugin();
  await plugin.event({
    event: { type: "session.created", properties: { info: { id: "cycle-a", parentID: "cycle-b" } } },
  });
  await plugin.event({
    event: { type: "session.created", properties: { info: { id: "cycle-b", parentID: "cycle-a" } } },
  });

  await plugin.event({ event: { type: "permission.asked", properties: { sessionID: "cycle-a" } } });

  expect(lifecycle().map(requestState)).toEqual(["blocked"]);
});

test("suppresses redundant same-session updates", async () => {
  const plugin = await loadPlugin();

  await plugin.event({
    event: {
      type: "session.status",
      properties: { sessionID: "root-session", status: { type: "busy" } },
    },
  });
  await plugin.event({
    event: { type: "session.updated", properties: { sessionID: "root-session" } },
  });
  await plugin.event({
    event: { type: "session.updated", properties: { sessionID: "replacement-session" } },
  });

  expect(lifecycle().map(requestMethod)).toEqual([
    "pane.report_agent",
    "pane.report_agent_session",
  ]);
  expect(lifecycle().map(requestSessionID)).toEqual(["root-session", "replacement-session"]);
});

test("does not classify server activity in another root session as a selection", async () => {
  const plugin = await loadPlugin();

  await busy(plugin, "visible-session");
  await busy(plugin, "attached-client-session");

  expect(lifecycle().map(requestMethod)).toEqual([
    "pane.report_agent",
    "pane.report_agent",
  ]);
  expect(lifecycle().map(requestSessionID)).toEqual([
    "visible-session",
    "attached-client-session",
  ]);
});

test("does not classify server-global root creation as a local selection", async () => {
  const plugin = await loadPlugin();

  await plugin.event({
    event: { type: "session.created", properties: { sessionID: "attached-session" } },
  });
  await plugin.event({
    event: { type: "session.updated", properties: { sessionID: "attached-session" } },
  });
  await busy(plugin, "attached-session");

  expect(lifecycle().map(requestMethod)).toEqual(["pane.report_agent"]);
  expect(lifecycle().map(requestSessionID)).toEqual(["attached-session"]);
});

test("reports retry status as working", async () => {
  const plugin = await loadPlugin();

  await plugin.event({
    event: {
      type: "session.status",
      properties: { sessionID: "root-session", status: { type: "retry" } },
    },
  });

  expect(lifecycle().map(requestMethod)).toEqual(["pane.report_agent"]);
  expect(lifecycle().map(requestState)).toEqual(["working"]);
  expect(lifecycle().map(requestSessionID)).toEqual(["root-session"]);
});

test("reports child prompts without replacing the root session", async () => {
  const plugin = await loadPlugin();

  // OpenCode maps `parentID: parent_id ?? undefined`, so the root's own
  // creation carries the key with no value.
  await plugin.event({
    event: {
      type: "session.created",
      properties: {
        sessionID: "root-session",
        info: { id: "root-session", parentID: undefined },
      },
    },
  });
  await plugin.event({
    event: {
      type: "session.created",
      properties: {
        info: { id: "child-session", parentID: "root-session" },
      },
    },
  });

  for (const type of ["permission.asked", "question.asked"]) {
    await plugin.event({ event: { type, properties: { sessionID: "child-session" } } });
  }
  for (const type of ["permission.replied", "question.replied", "question.rejected"]) {
    await plugin.event({ event: { type, properties: { sessionID: "child-session" } } });
  }

  expect(lifecycle().map(requestState)).toEqual([
    "blocked",
    "blocked",
    "working",
    "working",
    "working",
  ]);
  expect(lifecycle().map(requestSessionID)).toEqual([
    "root-session",
    "root-session",
    "root-session",
    "root-session",
    "root-session",
  ]);
});

test("routes nested child prompts to their own root, not the last active root", async () => {
  const plugin = await loadPlugin();
  for (const info of [
    { id: "root-session" },
    { id: "child-session", parentID: "root-session" },
    { id: "nested-session", parentID: "child-session" },
  ]) {
    await plugin.event({ event: { type: "session.created", properties: { info } } });
  }
  await busy(plugin, "other-root");
  await plugin.event({
    event: { type: "permission.asked", properties: { sessionID: "nested-session" } },
  });
  await plugin.event({
    event: { type: "permission.replied", properties: { sessionID: "nested-session" } },
  });
  await plugin.event({
    event: { type: "session.idle", properties: { sessionID: "nested-session" } },
  });
  await busy(plugin, "nested-session");

  expect(lifecycle().map(requestState)).toEqual(["working", "blocked", "working"]);
  expect(lifecycle().map(requestSessionID)).toEqual([
    "other-root",
    "root-session",
    "root-session",
  ]);
});

test("any parentID naming another session makes a child, even an unseen parent", async () => {
  const plugin = await loadPlugin();

  // A parentID equal to the session itself keeps it a root: its turns report
  // as its own. OpenCode never gives a root a parentID value, so only a broken
  // self-reference can reach here.
  await plugin.event({
    event: {
      type: "session.created",
      properties: { sessionID: "self-parent", info: { id: "self-parent", parentID: "self-parent" } },
    },
  });
  await busy(plugin, "self-parent");

  // A child whose parent's events never reached this process is still a child:
  // it must not report its own id as the pane's agent session. Its blocking
  // events resolve to the named parent, which is this pane's root.
  await plugin.event({
    event: {
      type: "session.created",
      properties: { sessionID: "stray", info: { id: "stray", parentID: "never-seen" } },
    },
  });
  await plugin.event({
    event: {
      type: "session.updated",
      properties: { sessionID: "stray", info: { id: "stray", parentID: "never-seen" } },
    },
  });
  await busy(plugin, "stray");
  await plugin.event({
    event: { type: "permission.asked", properties: { sessionID: "stray" } },
  });

  // A root seen only through status events still owns its children's prompts.
  await plugin.event({
    event: {
      type: "session.status",
      properties: { sessionID: "resumed", status: { type: "busy" } },
    },
  });
  await plugin.event({
    event: {
      type: "session.created",
      properties: { info: { id: "resumed-child", parentID: "resumed" } },
    },
  });
  await plugin.event({
    event: { type: "permission.asked", properties: { sessionID: "resumed-child" } },
  });
  await busy(plugin, "resumed-child");

  expect(lifecycle().map(requestMethod)).toEqual([
    "pane.report_agent",
    "pane.report_agent",
    "pane.report_agent",
    "pane.report_agent",
  ]);
  expect(lifecycle().map(requestState)).toEqual(["working", "blocked", "working", "blocked"]);
  expect(lifecycle().map(requestSessionID)).toEqual([
    "self-parent",
    "never-seen",
    "resumed",
    "resumed",
  ]);
  // The child's own id never becomes the pane's agent session.
  expect(lifecycle().map(requestSessionID)).not.toContain("stray");
});

// Activity hints ---------------------------------------------------------------

test("hints activity on session and todo events, including child sessions", async () => {
  const plugin = await loadPlugin();

  await plugin.event({
    event: {
      type: "session.created",
      properties: { sessionID: "root-session", info: { id: "root-session" } },
    },
  });
  await plugin.event({
    event: {
      type: "session.created",
      properties: { info: { id: "child-session", parentID: "root-session" } },
    },
  });
  await plugin.event({
    event: {
      type: "session.status",
      properties: { sessionID: "child-session", status: { type: "busy" } },
    },
  });
  await plugin.event({
    event: { type: "todo.updated", properties: { sessionID: "child-session", todos: [] } },
  });
  await plugin.event({
    event: { type: "permission.asked", properties: { sessionID: "child-session" } },
  });
  await plugin.event({
    event: { type: "session.idle", properties: { sessionID: "child-session" } },
  });
  await plugin.event({
    event: { type: "session.deleted", properties: { sessionID: "child-session" } },
  });
  await plugin.event({ event: { type: "tool.execute.before", properties: { sessionID: "root-session" } } });

  expect(activityHints()).toEqual([
    "session.created",
    "session.created",
    "session.status",
    "todo.updated",
    "session.idle",
    "session.deleted",
  ]);
  const activity = requests.filter(
    (request) => requestMethod(request) === "pane.report_agent_activity",
  );
  for (const request of activity) {
    expect(requestParam(request, "pane_id")).toBe("test:p1");
    expect(requestParam(request, "source")).toBe("herdr:opencode");
    expect(requestParam(request, "agent")).toBe("opencode");
  }
  const sequences = activity.map(requestSeq) as number[];
  for (let index = 1; index < sequences.length; index += 1) {
    expect(sequences[index]).toBe(sequences[index - 1] + 1);
  }
  // Child status changes hint the tree but never author the pane's lifecycle.
  expect(lifecycle().map(requestState)).toEqual(["blocked", "working"]);
  expect(lifecycle().map(requestSessionID)).toEqual(["root-session", "root-session"]);
});

test("collapses activity hints that arrive while a report is in flight", async () => {
  autoAcknowledge = false;
  const plugin = await loadPlugin();

  const firstHintDispatched = waitForNextRequest();
  const created = plugin.event({
    event: { type: "session.created", properties: { sessionID: "root-session" } },
  });
  await firstHintDispatched;
  // These three arrive before the next report can be dispatched: one queued
  // report carries the latest event name, and the state report follows it.
  const updated = plugin.event({
    event: { type: "session.updated", properties: { sessionID: "root-session" } },
  });
  const todo = plugin.event({
    event: { type: "todo.updated", properties: { sessionID: "root-session", todos: [] } },
  });
  const status = plugin.event({
    event: {
      type: "session.status",
      properties: { sessionID: "root-session", status: { type: "busy" } },
    },
  });
  expect(clients).toHaveLength(1);

  const secondHintDispatched = waitForNextRequest();
  clients[0]?.emit("data");
  await secondHintDispatched;
  expect(clients).toHaveLength(2);
  const stateDispatched = waitForNextRequest();
  clients[1]?.emit("data");
  await stateDispatched;
  expect(clients).toHaveLength(3);
  clients[2]?.emit("data");
  await Promise.all([created, updated, todo, status]);

  expect(activityHints()).toEqual(["session.created", "session.status"]);
  expect(lifecycle().map(requestState)).toEqual(["working"]);
  expect(requests.map(requestMethod)).toEqual([
    "pane.report_agent_activity",
    "pane.report_agent_activity",
    "pane.report_agent",
  ]);
});

test("only local run and Mini own server lifecycle, never shared servers or TUI workers", async () => {
  for (const args of [
    ["run"], ["run", "--session", "existing"], ["--mini"], ["--mini", "--session", "existing"],
    ["--print-logs", "--log-level", "DEBUG", "run"], ["run", "--", "--attach"],
  ]) {
    process.argv = ["bun", "/$bunfs/root/src/index.js", ...args];
    expect((await loadPlugin()).event).toBeFunction();
  }
  expect(requests).toHaveLength(0);

  // Everything else leaves lifecycle to the pane-local TUI plugin. fork: these
  // processes still send activity-tree hints (event names only), never state or
  // session reports.
  const nonOwners = [
    [], ["--session", "existing"], ["serve"], ["web"], ["attach", "http://localhost:4096"],
    ["run", "--attach", "http://localhost:4096"], ["--mini", "--attach=http://localhost:4096"],
    ["serve", "--", "--mini"],
  ].map((args) => ["bun", "/$bunfs/root/src/index.js", ...args]);
  nonOwners.push(["bun", "/$bunfs/root/src/cli/tui/worker.js"]);
  for (const argv of nonOwners) {
    process.argv = argv;
    const plugin = await loadPlugin();
    await plugin.event({
      event: {
        type: "session.status",
        properties: { sessionID: "root-session", status: { type: "busy" } },
      },
    });
    await plugin.event({
      event: { type: "permission.asked", properties: { sessionID: "root-session" } },
    });
    await plugin.event({ event: { type: "session.idle", properties: { sessionID: "root-session" } } });
  }
  expect(lifecycle()).toHaveLength(0);
  expect(activityHints()).toEqual(nonOwners.flatMap(() => ["session.status", "session.idle"]));
});

function requestMethod(request: unknown): unknown {
  return isRecord(request) ? request.method : undefined;
}

test("dual server entrypoint keeps V1 hooks and never reports from the V2 shared server", async () => {
  const module = await import(`./herdr-agent-state.js?test=${++importCounter}`);
  expect(module.default.server).toBe(module.HerdrAgentStatePlugin);
  expect(await module.default.setup({})).toBeUndefined();
  expect(requests).toHaveLength(0);
  const hooks = await module.default.server();
  expect(Object.keys(hooks)).toEqual(["event"]);
  await busy(hooks, "v1-root");
  expect(lifecycle().map(requestState)).toEqual(["working"]);
  expect(lifecycle().map(requestSessionID)).toEqual(["v1-root"]);
});

// Drives `working` the way OpenCode does when a turn runs in `sessionID`.
function busy(plugin: { event: (input: unknown) => Promise<unknown> }, sessionID: string) {
  return plugin.event({
    event: { type: "session.status", properties: { sessionID, status: { type: "busy" } } },
  });
}

function requestState(request: unknown): unknown {
  return requestParam(request, "state");
}

function requestSeq(request: unknown): unknown {
  return requestParam(request, "seq");
}

function requestSessionID(request: unknown): unknown {
  return requestParam(request, "agent_session_id");
}

function requestParam(request: unknown, name: string): unknown {
  if (!isRecord(request) || !isRecord(request.params)) {
    return undefined;
  }
  return request.params[name];
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}
