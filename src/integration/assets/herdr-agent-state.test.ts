import { afterEach, expect, test } from "bun:test";
import { rm } from "node:fs/promises";
import net, { createServer, type Server } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";

const originalPlatform = process.platform;
const originalCreateConnection = net.createConnection;
const originalEnvironment = {
  HERDR_ENV: process.env.HERDR_ENV,
  HERDR_PANE_ID: process.env.HERDR_PANE_ID,
  HERDR_SOCKET_PATH: process.env.HERDR_SOCKET_PATH,
};

let server: Server | undefined;
let socketPath: string | undefined;
let importCounter = 0;

afterEach(async () => {
  await new Promise<void>((resolve, reject) => {
    if (!server) {
      resolve();
      return;
    }
    server.close((error) => (error ? reject(error) : resolve()));
  });
  server = undefined;

  if (socketPath) {
    await rm(socketPath, { force: true });
    socketPath = undefined;
  }

  Object.defineProperty(process, "platform", { value: originalPlatform });
  net.createConnection = originalCreateConnection;
  for (const [name, value] of Object.entries(originalEnvironment)) {
    if (value === undefined) {
      delete process.env[name];
    } else {
      process.env[name] = value;
    }
  }
});

const integrations = [
  { name: "Pi", modulePath: "./pi/herdr-agent-state.ts" },
] as const;

const socketPlugins = [
  {
    name: "OpenCode",
    modulePath: "./opencode/herdr-agent-state.js",
    sessionID: "opencode-session",
  },
] as const;

function importFresh(modulePath: string) {
  importCounter += 1;
  return import(`${modulePath}?test=${importCounter}`);
}

type Handler = (event: unknown, context: unknown) => unknown;

function createExtensionHarness() {
  const handlers = new Map<string, Handler>();
  const eventHandlers = new Map<string, Handler>();
  return {
    handlers,
    eventHandlers,
    pi: {
      on(event: string, handler: Handler) {
        handlers.set(event, handler);
      },
      events: {
        on(event: string, handler: Handler) {
          eventHandlers.set(event, handler);
          return () => {};
        },
      },
    },
  };
}

function configureIntegrationEnvironment(recordingSocketPath: string) {
  process.env.HERDR_ENV = "1";
  process.env.HERDR_SOCKET_PATH = recordingSocketPath;
  process.env.HERDR_PANE_ID = "test:p1";
}

function captureConnectionEndpoint() {
  let connectedEndpoint: unknown;
  net.createConnection = ((...args: unknown[]) => {
    connectedEndpoint = args[0];
    return Reflect.apply(originalCreateConnection, net, args);
  }) as typeof net.createConnection;
  return () => connectedEndpoint;
}

async function startRecordingServer(name: string): Promise<unknown[]> {
  const recordingSocketPath = join(tmpdir(), `herdr-${name}-${process.pid}.sock`);
  socketPath = recordingSocketPath;
  await rm(recordingSocketPath, { force: true });

  const requests: unknown[] = [];
  const recordingServer = createServer((socket) => {
    let input = "";
    socket.setEncoding("utf8");
    socket.on("data", (chunk) => {
      input += chunk;
      const newline = input.indexOf("\n");
      if (newline === -1) {
        return;
      }
      requests.push(JSON.parse(input.slice(0, newline)));
      socket.end("{}\n");
    });
  });
  server = recordingServer;
  await new Promise<void>((resolve, reject) => {
    recordingServer.once("error", reject);
    recordingServer.listen(originalPlatform === "win32" ? `\\\\.\\pipe\\${recordingSocketPath}` : recordingSocketPath, resolve);
  });
  configureIntegrationEnvironment(recordingSocketPath);
  return requests;
}

// Like `startRecordingServer`, but the first `dropActivity` activity requests
// are closed without an answer (Herdr unreachable mid-send), `dropDelayMs`
// after they arrive (the request stays in flight until then). Every attempt is
// recorded, answered or not.
async function startFlakyActivityServer(name: string, dropActivity: number, dropDelayMs = 0): Promise<unknown[]> {
  const recordingSocketPath = join(tmpdir(), `herdr-${name}-${process.pid}.sock`);
  socketPath = recordingSocketPath;
  await rm(recordingSocketPath, { force: true });

  const requests: unknown[] = [];
  let dropped = 0;
  const recordingServer = createServer((socket) => {
    let input = "";
    socket.setEncoding("utf8");
    socket.on("data", (chunk) => {
      input += chunk;
      const newline = input.indexOf("\n");
      if (newline === -1) {
        return;
      }
      const request = JSON.parse(input.slice(0, newline));
      requests.push(request);
      if (request.method === "pane.report_agent_activity" && dropped < dropActivity) {
        dropped += 1;
        if (dropDelayMs > 0) {
          setTimeout(() => socket.end(), dropDelayMs);
        } else {
          socket.end();
        }
        return;
      }
      socket.end("{}\n");
    });
  });
  server = recordingServer;
  await new Promise<void>((resolve, reject) => {
    recordingServer.once("error", reject);
    recordingServer.listen(recordingSocketPath, resolve);
  });
  configureIntegrationEnvironment(recordingSocketPath);
  return requests;
}

for (const socketPlugin of socketPlugins) {
  test(`${socketPlugin.name} maps the Windows socket marker path to a named pipe endpoint`, async () => {
    const markerPath = `herdr-${socketPlugin.name.toLowerCase()}-${process.pid}.sock`;
    configureIntegrationEnvironment(markerPath);
    Object.defineProperty(process, "platform", { value: "win32" });
    const connectedEndpoint = captureConnectionEndpoint();

    const { HerdrAgentStatePlugin } = await importFresh(socketPlugin.modulePath);
    const plugin = await HerdrAgentStatePlugin();
    await plugin.event({
      event: {
        type: "session.updated",
        properties: { sessionID: socketPlugin.sessionID },
      },
    });

    expect(connectedEndpoint()).toBe(`\\\\.\\pipe\\${markerPath}`);
  });
}

test("OpenCode stays disabled without the Herdr socket environment", async () => {
  process.env.HERDR_ENV = "1";
  process.env.HERDR_PANE_ID = "test:p1";
  delete process.env.HERDR_SOCKET_PATH;

  const { HerdrAgentStatePlugin } = await importFresh("./opencode/herdr-agent-state.js");

  expect(await HerdrAgentStatePlugin()).toEqual({});
});

for (const integration of integrations) {
  test(`${integration.name} maps the Windows socket marker path to a named pipe endpoint`, async () => {
    const markerPath = `herdr-${integration.name.toLowerCase().replaceAll(" ", "-")}-${process.pid}.sock`;
    configureIntegrationEnvironment(markerPath);
    Object.defineProperty(process, "platform", { value: "win32" });
    const connectedEndpoint = captureConnectionEndpoint();
    const { handlers, pi } = createExtensionHarness();

    const { default: install } = await importFresh(integration.modulePath);
    install(pi);
    await handlers.get("session_start")?.(
      { reason: "startup" },
      {
        hasUI: true,
        mode: "tui",
        isIdle: () => true,
        sessionManager: {
          getSessionFile: () => undefined,
          getSessionId: () => "test-session",
        },
      },
    );

    expect(connectedEndpoint()).toBe(`\\\\.\\pipe\\${markerPath}`);
  });

  test(`${integration.name} reload preserves working state when the agent is active`, async () => {
    const requests = await startRecordingServer(
      integration.name.toLowerCase().replaceAll(" ", "-"),
    );
    const { handlers, pi } = createExtensionHarness();

    const { default: install } = await importFresh(integration.modulePath);
    install(pi);

    const sessionStart = handlers.get("session_start");
    expect(sessionStart).toBeDefined();
    await sessionStart?.(
      { reason: "reload" },
      {
        hasUI: true,
        mode: "tui",
        isIdle: () => false,
        sessionManager: {
          getSessionFile: () => undefined,
          getSessionId: () => undefined,
        },
      },
    );

    const reportedState = () => {
      for (const request of requests) {
        if (!isRecord(request) || request.method !== "pane.report_agent") {
          continue;
        }
        const params = request.params;
        if (isRecord(params) && typeof params.state === "string") {
          return params.state;
        }
      }
      return undefined;
    };

    const deadline = Date.now() + 1_000;
    while (Date.now() < deadline && reportedState() === undefined) {
      await Bun.sleep(5);
    }

    expect(reportedState()).toBe("working");
  });
}

test("Pi reports a Windows session path", async () => {
  const requests = await startRecordingServer("pi-windows-session-path");
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  const sessionPath = "C:\\Users\\User\\.pi\\agent\\sessions\\pi-session.jsonl";
  await handlers.get("session_start")?.(
    { reason: "startup" },
    {
      ...piContext(() => true),
      sessionManager: {
        getSessionFile: () => sessionPath,
        getSessionId: () => "pi-session",
      },
    },
  );
  await waitFor(() => requests.length === 2);

  expect(requests.map(requestSessionPath)).toEqual([sessionPath, sessionPath]);
});

test("Pi reports idle only after the agent settles", async () => {
  const requests = await startRecordingServer("pi-settled");
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  expect(completionHandlers(handlers)).toEqual(["agent_settled"]);
  let idle = true;
  const context = piContext(() => idle);
  await handlers.get("session_start")?.({ reason: "startup" }, context);
  await waitFor(() => requestStates(requests).length === 1);

  idle = false;
  handlers.get("agent_start")?.({}, context);
  await waitFor(() => requestStates(requests).length === 2);
  expect(requestStates(requests)).toEqual(["idle", "working"]);
  expect(handlers.has("agent_end")).toBe(false);

  const requestCountBeforeStaleSettlement = requests.length;
  handlers.get("agent_settled")?.({}, context);
  await Bun.sleep(25);
  expect(requests).toHaveLength(requestCountBeforeStaleSettlement);
  expect(requestStates(requests)).toEqual(["idle", "working"]);

  idle = true;
  handlers.get("agent_settled")?.({}, context);
  await waitFor(() => requestStates(requests).length === 3);
  expect(requestStates(requests)).toEqual(["idle", "working", "idle"]);
});

test("Pi ignores RPC sessions even when UI APIs are available", async () => {
  const requests = await startRecordingServer("pi-rpc");
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  const context = {
    ...piContext(() => true),
    hasUI: true,
    mode: "rpc",
  };
  await handlers.get("session_start")?.({ reason: "startup" }, context);
  handlers.get("agent_start")?.({}, context);
  handlers.get("agent_settled")?.({}, context);
  await Bun.sleep(25);

  expect(requests).toEqual([]);
});

test("Pi settlement preserves explicit blocked-state precedence", async () => {
  const requests = await startRecordingServer("pi-settled-blocked");
  const { eventHandlers, handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  let idle = true;
  const context = piContext(() => idle);
  await handlers.get("session_start")?.({ reason: "startup" }, context);
  await waitFor(() => requestStates(requests).length === 1);
  idle = false;
  handlers.get("agent_start")?.({}, context);
  await waitFor(() => requestStates(requests).length === 2);
  eventHandlers.get("herdr:blocked")?.({ active: true, label: "approval" }, context);
  await waitFor(() => requestStates(requests).length === 3);

  idle = true;
  handlers.get("agent_settled")?.({}, context);
  await Bun.sleep(25);
  expect(requestStates(requests)).toEqual(["idle", "working", "blocked"]);

  eventHandlers.get("herdr:blocked")?.({ active: false }, context);
  await waitFor(() => requestStates(requests).length === 4);
  expect(requestStates(requests)).toEqual(["idle", "working", "blocked", "idle"]);
});

test("Pi reports the session replacement source", async () => {
  const requests = await startRecordingServer("pi-session-source");
  const { handlers, pi } = createExtensionHarness();

  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  const sessionStart = handlers.get("session_start");
  expect(sessionStart).toBeDefined();
  await sessionStart?.(
    { reason: "new" },
    {
      hasUI: true,
      mode: "tui",
      isIdle: () => true,
      sessionManager: {
        getSessionFile: () => "/tmp/pi-new.jsonl",
        getSessionId: () => "pi-new",
      },
    },
  );

  const reportedSession = () =>
    requests.find((request) => isRecord(request) && request.method === "pane.report_agent_session");
  const deadline = Date.now() + 1_000;
  while (Date.now() < deadline && reportedSession() === undefined) {
    await Bun.sleep(5);
  }

  const request = reportedSession();
  expect(request).toBeDefined();
  expect(isRecord(request) && isRecord(request.params) ? request.params.session_start_source : null)
    .toBe("new");
});

test("Pi waits for a replacement session report before publishing state", async () => {
  const recordingSocketPath = join(tmpdir(), `herdr-pi-session-order-${process.pid}.sock`);
  socketPath = recordingSocketPath;
  await rm(recordingSocketPath, { force: true });

  const requests: unknown[] = [];
  let acknowledgeSessionReport: (() => void) | undefined;
  const recordingServer = createServer((socket) => {
    let input = "";
    socket.setEncoding("utf8");
    socket.on("data", (chunk) => {
      input += chunk;
      const newline = input.indexOf("\n");
      if (newline === -1) {
        return;
      }
      const request = JSON.parse(input.slice(0, newline));
      requests.push(request);
      if (isRecord(request) && request.method === "pane.report_agent_session") {
        acknowledgeSessionReport = () => socket.end("{}\n");
        return;
      }
      socket.end("{}\n");
    });
  });
  server = recordingServer;
  await new Promise<void>((resolve, reject) => {
    recordingServer.once("error", reject);
    recordingServer.listen(originalPlatform === "win32" ? `\\\\.\\pipe\\${recordingSocketPath}` : recordingSocketPath, resolve);
  });

  configureIntegrationEnvironment(recordingSocketPath);
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  const sessionStart = handlers.get("session_start");
  expect(sessionStart).toBeDefined();
  const sessionStartResult = sessionStart?.(
    { reason: "new" },
    {
      hasUI: true,
      mode: "tui",
      isIdle: () => false,
      sessionManager: {
        getSessionFile: () => "/tmp/pi-new.jsonl",
        getSessionId: () => "pi-new",
      },
    },
  );

  const deadline = Date.now() + 1_000;
  while (Date.now() < deadline && acknowledgeSessionReport === undefined) {
    await Bun.sleep(5);
  }
  expect(acknowledgeSessionReport).toBeDefined();
  expect(
    requests.some((request) => isRecord(request) && request.method === "pane.report_agent"),
  ).toBe(false);

  acknowledgeSessionReport?.();
  await sessionStartResult;

  const stateDeadline = Date.now() + 1_000;
  while (
    Date.now() < stateDeadline &&
    !requests.some((request) => isRecord(request) && request.method === "pane.report_agent")
  ) {
    await Bun.sleep(5);
  }
  expect(requests.map((request) => (isRecord(request) ? request.method : undefined))).toEqual([
    "pane.report_agent_session",
    "pane.report_agent",
  ]);
});

async function startDroppedFirstResponseServer(name: string) {
  const recordingSocketPath = join(tmpdir(), `herdr-${name}-${process.pid}.sock`);
  socketPath = recordingSocketPath;
  await rm(recordingSocketPath, { force: true });

  let connectionCount = 0;
  const attemptedRequests: unknown[] = [];
  const deliveredRequests: unknown[] = [];
  const recordingServer = createServer((socket) => {
    connectionCount += 1;
    const connectionNumber = connectionCount;
    let input = "";
    socket.setEncoding("utf8");
    socket.on("data", (chunk) => {
      input += chunk;
      const newline = input.indexOf("\n");
      if (newline === -1) {
        return;
      }
      const request = JSON.parse(input.slice(0, newline));
      attemptedRequests.push(request);
      if (connectionNumber === 1) {
        return;
      }
      deliveredRequests.push(request);
      socket.end("{}\n");
    });
  });
  server = recordingServer;
  await new Promise<void>((resolve, reject) => {
    recordingServer.once("error", reject);
    recordingServer.listen(originalPlatform === "win32" ? `\\\\.\\pipe\\${recordingSocketPath}` : recordingSocketPath, resolve);
  });

  configureIntegrationEnvironment(recordingSocketPath);
  return {
    attemptedRequests,
    deliveredRequests,
    connectionCount: () => connectionCount,
  };
}

test("Pi retries working state after an unanswered socket attempt", async () => {
  const { attemptedRequests, deliveredRequests, connectionCount } =
    await startDroppedFirstResponseServer("pi-retry");
  const { handlers, pi } = createExtensionHarness();

  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  const sessionStart = handlers.get("session_start");
  expect(sessionStart).toBeDefined();
  await sessionStart?.(
    { reason: "startup" },
    {
      hasUI: true,
      mode: "tui",
      isIdle: () => false,
      sessionManager: {
        getSessionFile: () => undefined,
        getSessionId: () => undefined,
      },
    },
  );

  const reportedWorking = () =>
    deliveredRequests.some((request) => {
      if (!isRecord(request) || request.method !== "pane.report_agent") {
        return false;
      }
      const params = request.params;
      return isRecord(params) && params.state === "working";
    });

  const deadline = Date.now() + 2_500;
  while (Date.now() < deadline && !reportedWorking()) {
    await Bun.sleep(5);
  }

  expect(connectionCount()).toBeGreaterThanOrEqual(2);
  expect(attemptedRequests.length).toBeGreaterThanOrEqual(2);
  expect(attemptedRequests[1]).toEqual(attemptedRequests[0]);
  expect(reportedWorking()).toBe(true);
});

function assistantEntry(overrides: Record<string, unknown> = {}) {
  return {
    type: "message",
    message: {
      role: "assistant",
      provider: "anthropic",
      model: "claude-sonnet-4-5",
      // Session entries carry cost as an object; `total` is the billed amount.
      usage: {
        input: 1000,
        output: 200,
        cacheRead: 3000,
        cacheWrite: 40,
        totalTokens: 4240,
        cost: { input: 0.1, output: 0.2, cacheRead: 0.05, cacheWrite: 0.01, total: 0.36 },
      },
      ...overrides,
    },
  };
}

function piUsageContext(options: {
  isIdle?: () => boolean;
  mode?: string;
  contextUsage?: unknown;
  entries?: unknown[];
  model?: unknown;
}) {
  return {
    ...piContext(options.isIdle ?? (() => true)),
    mode: options.mode ?? "tui",
    model: options.model,
    getContextUsage: () => options.contextUsage,
    sessionManager: {
      getSessionFile: () => undefined,
      getSessionId: () => undefined,
      getEntries: () => options.entries ?? [],
    },
  };
}

function usageReports(requests: unknown[]): Record<string, unknown>[] {
  return requests
    .filter((request) => isRecord(request) && request.method === "account.usage.report")
    .map((request) => (request as { params: Record<string, unknown> }).params);
}

test("Pi pushes session usage with the billed provider and model", async () => {
  const requests = await startRecordingServer("pi-usage-push");
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  expect(handlers.has("turn_end")).toBe(true);
  expect(handlers.has("session_compact")).toBe(true);

  const entries: unknown[] = [];
  const context = piUsageContext({
    contextUsage: { tokens: 45_000, contextWindow: 200_000, percent: 22.5 },
    entries,
    model: { provider: "anthropic", id: "claude-sonnet-4-5" },
  });
  await handlers.get("session_start")?.({ reason: "startup" }, context);
  await waitFor(() => usageReports(requests).length === 1);
  // No assistant usage yet: only the context gauge and the configured model.
  expect(usageReports(requests)[0]).toEqual({
    agent: "pi",
    pane_id: "test:p1",
    official_payload: {
      source: "herdr:pi",
      version: 1,
      provider: "anthropic",
      model: "claude-sonnet-4-5",
      context: { tokens: 45_000, percent: 22.5, context_window: 200_000 },
    },
  });

  // The provider answered with a different model than requested: bill that one.
  entries.push(assistantEntry({ responseModel: "claude-sonnet-4-5-20250929" }));
  entries.push({ type: "message", message: { role: "user", content: "hi" } });
  entries.push({
    type: "compaction",
    usage: { input: 10, output: 5, cacheRead: 0, cacheWrite: 0, cost: { total: 0.04 } },
  });
  handlers.get("agent_settled")?.({}, context);
  await waitFor(() => usageReports(requests).length === 2);
  const payload = usageReports(requests)[1].official_payload as Record<string, unknown>;
  expect(payload.model).toBe("claude-sonnet-4-5-20250929");
  expect(payload.tokens).toEqual({
    input: 1010,
    output: 205,
    cache_read: 3000,
    cache_write: 40,
    total: 4255,
  });
  // `0.36 + 0.04` sums to `0.39999999999999997` in IEEE754. Herdr computes this
  // total itself, and the account page renders the decimal verbatim, so the wire
  // value must already be rounded — pin the serialized digits, not an epsilon.
  expect(payload.cost_usd).toBe(0.4);
  expect(JSON.stringify(payload.cost_usd)).toBe("0.4");
  // Herdr's report contract takes a number, never Pi's cost object.
  expect(typeof payload.cost_usd).toBe("number");
});

test("Pi reports unknown context after compaction as null, not zero", async () => {
  const requests = await startRecordingServer("pi-usage-compaction");
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  let contextUsage: unknown = { tokens: 0, contextWindow: 200_000, percent: 0 };
  const context = {
    ...piUsageContext({ entries: [assistantEntry()] }),
    getContextUsage: () => contextUsage,
  };
  await handlers.get("session_start")?.({ reason: "startup" }, context);
  await waitFor(() => usageReports(requests).length === 1);
  expect((usageReports(requests)[0].official_payload as Record<string, unknown>).context).toEqual({
    tokens: 0,
    percent: 0,
    context_window: 200_000,
  });

  contextUsage = { tokens: null, contextWindow: 200_000, percent: null };
  handlers.get("session_compact")?.({}, context);
  await waitFor(() => usageReports(requests).length === 2);
  expect((usageReports(requests)[1].official_payload as Record<string, unknown>).context).toEqual({
    tokens: null,
    percent: null,
    context_window: 200_000,
  });
});

test("Pi reads cost from the session object shape and the RPC number shape separately", async () => {
  const { usageCostUsd, collectUsagePayload } = await importFresh("./pi/herdr-agent-state.ts");

  // Session entries / JSON mode: `usage.cost` is an object.
  expect(usageCostUsd({ cost: { input: 0.1, output: 0.2, total: 0.3 } })).toBe(0.3);
  // RPC `get_session_stats`: `cost` is a plain number.
  expect(usageCostUsd({ cost: 1.25 })).toBe(1.25);
  // An object is never coerced into a number, and junk never becomes NaN.
  expect(usageCostUsd({ cost: { input: 0.1 } })).toBe(0);
  expect(usageCostUsd({ cost: "1.25" })).toBe(0);
  expect(usageCostUsd({ cost: Number.NaN })).toBe(0);
  expect(usageCostUsd({})).toBe(0);
  expect(usageCostUsd(undefined)).toBe(0);

  const payload = collectUsagePayload(
    piUsageContext({
      entries: [assistantEntry(), { type: "usage", provider: "x", model: "y", usage: { input: 1, output: 1, cacheRead: 0, cacheWrite: 0, cost: 0.5 } }],
    }),
  );
  expect(payload?.cost_usd).toBe(0.86);
  expect(JSON.stringify(payload?.cost_usd)).toBe("0.86");

  // `0.1 + 0.2` is the textbook IEEE754 tail: the reported total must carry at
  // most 6 decimals so the account page never shows `0.30000000000000004 USD`.
  const tailing = collectUsagePayload(
    piUsageContext({
      entries: [
        { type: "usage", usage: { input: 1, output: 0, cacheRead: 0, cacheWrite: 0, cost: 0.1 } },
        { type: "usage", usage: { input: 1, output: 0, cacheRead: 0, cacheWrite: 0, cost: 0.2 } },
      ],
    }),
  );
  expect(tailing?.cost_usd).toBe(0.3);
  expect(JSON.stringify(tailing?.cost_usd)).toBe("0.3");

  // Nothing to report: no context gauge and no usage entries.
  expect(collectUsagePayload(piUsageContext({}))).toBeUndefined();
  expect(collectUsagePayload(undefined)).toBeUndefined();
});

test("Pi throttles per-turn usage pushes and never pushes from headless modes", async () => {
  const requests = await startRecordingServer("pi-usage-throttle");
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  const entries: unknown[] = [assistantEntry()];
  let idle = false;
  const context = piUsageContext({ isIdle: () => idle, entries });
  await handlers.get("session_start")?.({ reason: "startup" }, context);
  await waitFor(() => usageReports(requests).length === 1);

  // Turns inside the throttle window are dropped; the settled push carries the latest totals.
  entries.push(assistantEntry());
  handlers.get("turn_end")?.({}, context);
  entries.push(assistantEntry());
  handlers.get("turn_end")?.({}, context);
  await Bun.sleep(25);
  expect(usageReports(requests)).toHaveLength(1);

  idle = true;
  handlers.get("agent_settled")?.({}, context);
  await waitFor(() => usageReports(requests).length === 2);
  const settled = usageReports(requests)[1].official_payload as { tokens: { input: number } };
  expect(settled.tokens.input).toBe(3000);

  // Identical data is not pushed twice.
  handlers.get("agent_settled")?.({}, context);
  await Bun.sleep(25);
  expect(usageReports(requests)).toHaveLength(2);
});

test("Pi never pushes usage from RPC sessions", async () => {
  const requests = await startRecordingServer("pi-usage-rpc");
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  const context = piUsageContext({
    mode: "rpc",
    contextUsage: { tokens: 1, contextWindow: 10, percent: 10 },
    entries: [assistantEntry()],
  });
  await handlers.get("session_start")?.({ reason: "startup" }, context);
  handlers.get("turn_end")?.({}, context);
  handlers.get("session_compact")?.({}, context);
  handlers.get("agent_settled")?.({}, context);
  await Bun.sleep(25);

  expect(requests).toEqual([]);
});

// Activity snapshot ----------------------------------------------------------

const PI_ACTIVITY_FIXTURE = join(
  import.meta.dir,
  "../../../tests/fixtures/agent-activity/pi/snapshot-extension.json",
);

function toolCatalog(entries: Record<string, string>) {
  return () =>
    Object.entries(entries).map(([name, source]) => ({
      name,
      description: name,
      parameters: {},
      sourceInfo: { path: `<${source}:${name}>`, source, scope: "user", origin: "top-level" },
    }));
}

function assistant(text: string, stopReason = "toolUse") {
  return { role: "assistant", content: [{ type: "text", text }], stopReason };
}

function delegated(agent: string, task: string, fields: Record<string, unknown> = {}) {
  return { agent, agentSource: "user", task, exitCode: 0, messages: [], stderr: "", ...fields };
}

function activityRequests(requests: unknown[]): Record<string, unknown>[] {
  return requests
    .filter((request) => isRecord(request) && request.method === "pane.report_agent_activity")
    .map((request) => (request as { params: Record<string, unknown> }).params);
}

function activityNodes(params: Record<string, unknown>): Record<string, unknown>[] {
  return (JSON.parse(params.hint as string) as { nodes: Record<string, unknown>[] }).nodes;
}

// Replays one parallel subagent call (the official example's details shape),
// a failed extension tool, a built-in tool and a running tool with CJK output
// past the tail limit. The golden file is also the Rust adapter's fixture.
function scriptedActivitySnapshot(createActivityTracker: (pi: unknown) => any): string {
  const tracker = createActivityTracker({
    getAllTools: toolCatalog({
      bash: "builtin",
      read: "builtin",
      subagent: "/home/user/.pi/agent/extensions/subagent/index.ts",
      web_search: "/home/user/.pi/agent/extensions/web-search.ts",
      notes: "sdk",
    }),
  });
  const tasks = [
    { agent: "scout", task: "Map the auth module" },
    { agent: "reviewer", task: "Review the login flow\nFocus on token refresh" },
    { agent: "planner", task: "Draft a migration plan" },
  ];
  const call = { toolCallId: "call_par", toolName: "subagent", args: { tasks } };
  const placeholders = tasks.map((t) => delegated(t.agent, t.task, { exitCode: -1 }));
  const update = (results: unknown[], text: string) =>
    tracker.update(
      {
        ...call,
        partialResult: {
          content: [{ type: "text", text }],
          details: { mode: "parallel", agentScope: "user", projectAgentsDir: null, results },
        },
      },
      2_000,
    );

  tracker.start(call, 1_000);
  update(placeholders, "Parallel: 0/3 done, 3 running...");
  const scoutRunning = delegated("scout", tasks[0].task, {
    messages: [assistant("Reading src/auth to find the session store.")],
  });
  update([scoutRunning, placeholders[1], placeholders[2]], "Parallel: 0/3 done, 3 running...");
  const scoutDone = delegated("scout", tasks[0].task, {
    messages: [
      assistant("Reading src/auth to find the session store."),
      assistant("The session store lives in src/auth/session.ts.", "stop"),
    ],
  });
  const reviewerFailed = delegated("reviewer", tasks[1].task, {
    exitCode: 1,
    stderr: "provider rejected the request",
    messages: [assistant("Checking the refresh path.")],
  });
  const plannerRunning = delegated("planner", tasks[2].task, {
    messages: [assistant("Listing the tables that change.")],
  });
  update([scoutDone, reviewerFailed, plannerRunning], "Parallel: 2/3 done, 1 running...");
  tracker.start({ toolCallId: "call_bash", toolName: "bash", args: { command: "ls" } }, 2_500);
  tracker.end(
    {
      ...call,
      result: {
        content: [{ type: "text", text: "Parallel: 1/3 succeeded" }],
        details: {
          mode: "parallel",
          results: [
            scoutDone,
            reviewerFailed,
            delegated("planner", tasks[2].task, {
              messages: [assistant("Plan: add a nullable column first.", "stop")],
            }),
          ],
        },
      },
      isError: false,
    },
    3_000,
  );

  const search = { toolCallId: "call_ws", toolName: "web_search", args: { query: "ratatui tree widget" } };
  tracker.start(search, 4_000);
  tracker.update({ ...search, partialResult: { content: [{ type: "text", text: "Searching" }] } }, 4_100);
  tracker.update({ ...search, partialResult: { content: [{ type: "text", text: "Searching..." }] } }, 4_200);
  tracker.end(
    { ...search, result: { content: [{ type: "text", text: "\u001b[31mrate limited\u001b[0m" }], isError: true }, isError: false },
    4_300,
  );

  const notes = { toolCallId: "call_notes", toolName: "notes", args: { title: "发布检查清单" } };
  tracker.start(notes, 5_000);
  const line = "检查点：确认迁移脚本可回滚。\n";
  tracker.update({ ...notes, partialResult: { content: [{ type: "text", text: line.repeat(150) }] } }, 5_100);

  return tracker.snapshot({
    session_path: "/home/user/.pi/agent/sessions/--home-user-demo--/2026-09-22T10-00-00-000Z_5f000000-0000-4000-8000-000000000001.jsonl",
    session_id: "5f000000-0000-4000-8000-000000000001",
  });
}

test("Pi activity snapshot matches the adapter fixture", async () => {
  const { createActivityTracker } = await importFresh("./pi/herdr-agent-state.ts");
  const hint = scriptedActivitySnapshot(createActivityTracker);
  if (process.env.HERDR_UPDATE_PI_ACTIVITY_FIXTURE === "1") {
    await Bun.write(PI_ACTIVITY_FIXTURE, `${JSON.stringify(JSON.parse(hint), null, 2)}\n`);
  }
  expect(JSON.parse(hint)).toEqual(await Bun.file(PI_ACTIVITY_FIXTURE).json());

  const snapshot = JSON.parse(hint);
  expect(snapshot.type).toBe("herdr.activity.snapshot");
  expect(snapshot.version).toBe(1);
  const byId = new Map(snapshot.nodes.map((node: { id: string }) => [node.id, node]));
  // The built-in tool never becomes a node; parents precede their children.
  expect(snapshot.nodes.map((node: { id: string }) => node.id)).toEqual([
    "call_par",
    "call_par/0",
    "call_par/1",
    "call_par/2",
    "call_ws",
    "call_notes",
  ]);
  expect(byId.get("call_par")).toMatchObject({ kind: "subagent", status: "done", ended_at_ms: 3_000 });
  expect(byId.get("call_par/0")).toMatchObject({ status: "done", agent_type: "scout", parent_id: "call_par" });
  expect(byId.get("call_par/1")).toMatchObject({ status: "failed", summary: "provider rejected the request" });
  expect(byId.get("call_par/2")).toMatchObject({ status: "done", ended_at_ms: 3_000 });
  // `isError` inside the returned result marks the tool failed; ANSI is stripped.
  expect(byId.get("call_ws")).toMatchObject({ kind: "task", status: "failed", summary: "rate limited" });
  expect((byId.get("call_ws") as { output: { text: string } }).output.text).toBe(
    "Searching...\n\nrate limited",
  );
  // The head of a long log is dropped; `start` counts the dropped UTF-8 bytes.
  const notesOutput = (byId.get("call_notes") as { output: { text: string; start: number } }).output;
  expect(notesOutput.text.length).toBe(2_000);
  expect(notesOutput.start).toBe(Buffer.byteLength("检查点：确认迁移脚本可回滚。\n".repeat(150), "utf8") -
    Buffer.byteLength(notesOutput.text, "utf8"));
});

test("Pi activity tracks extension tools only", async () => {
  const { isExtensionTool } = await importFresh("./pi/herdr-agent-state.ts");
  const pi = {
    getAllTools: toolCatalog({ bash: "user-sandbox", subagent: "ext", web_fetch: "builtin", notes: "sdk" }),
  };
  // An override of a built-in name is still the everyday tool.
  expect(isExtensionTool(pi, "bash")).toBe(false);
  expect(isExtensionTool(pi, "web_fetch")).toBe(false);
  expect(isExtensionTool(pi, "subagent")).toBe(true);
  expect(isExtensionTool(pi, "notes")).toBe(true);
  // Unknown to the catalog (or no catalog at all): only the built-in names are excluded.
  expect(isExtensionTool(pi, "late_tool")).toBe(true);
  expect(isExtensionTool({}, "subagent")).toBe(true);
  expect(isExtensionTool({}, "read")).toBe(false);
  expect(isExtensionTool({ getAllTools: () => { throw new Error("not bound"); } }, "subagent")).toBe(true);
  expect(isExtensionTool(pi, "")).toBe(false);
  expect(isExtensionTool(pi, undefined)).toBe(false);
});

test("Pi activity log appends growth and keeps replaced text", async () => {
  const { createActivityTracker, ACTIVITY_OUTPUT_CHANGED, ACTIVITY_TREE_CHANGED } =
    await importFresh("./pi/herdr-agent-state.ts");
  const tracker = createActivityTracker({});
  const call = { toolCallId: "call_1", toolName: "research", args: { prompt: "Find \u001b[1mall\u001b[0m callers\nof foo" } };
  expect(tracker.start(call, 10)).toBe(ACTIVITY_TREE_CHANGED);
  expect(tracker.start(call, 11)).toBe(0);
  const text = (value: string) => ({ ...call, partialResult: { content: [{ type: "text", text: value }] } });
  expect(tracker.update(text("Step 1"), 12)).toBe(ACTIVITY_OUTPUT_CHANGED);
  expect(tracker.update(text("Step 1"), 13)).toBe(0);
  expect(tracker.update(text("Step 1 done"), 14)).toBe(ACTIVITY_OUTPUT_CHANGED);
  // A lone surrogate never reaches the wire; carriage returns become newlines.
  expect(tracker.update(text("Step 2\r\nbad \ud800 half"), 15)).toBe(ACTIVITY_OUTPUT_CHANGED);

  const [node] = JSON.parse(tracker.snapshot({})).nodes;
  expect(node).toEqual({
    id: "call_1",
    kind: "task",
    label: "research Find all callers",
    status: "running",
    summary: "bad � half",
    started_at_ms: 10,
    output: { text: "Step 1 done\n\nStep 2\nbad � half", start: 0, format: "text" },
  });
  const final = { content: [{ type: "text", text: "Found 3 callers\n- a.ts\n- b.ts" }] };
  expect(tracker.end({ ...call, result: final, isError: false }, 20)).toBe(ACTIVITY_TREE_CHANGED);
  // Late events for a finished tool change nothing.
  expect(tracker.update(text("late"), 21)).toBe(0);
  expect(tracker.end({ ...call, result: {}, isError: true }, 22)).toBe(0);
  // A finished node is summarized by the headline of its final text.
  expect(JSON.parse(tracker.snapshot({})).nodes[0]).toMatchObject({
    status: "done",
    ended_at_ms: 20,
    summary: "Found 3 callers",
  });
});

test("Pi activity snapshot never carries a lone surrogate", async () => {
  const { createActivityTracker } = await importFresh("./pi/herdr-agent-state.ts");
  const tracker = createActivityTracker({});
  // Lone halves in the call id, the agent name and a delegated agent's name.
  const call = { toolCallId: "call_\ud800x", toolName: "subagent", args: { agent: "sc\udc00out\u001b[1m", tasks: [{}] } };
  tracker.start(call, 1);
  tracker.update(
    { ...call, partialResult: { content: [], details: { mode: "parallel", results: [delegated("rev\ud83dewer", "Review")] } } },
    2,
  );
  tracker.end({ ...call, result: { content: [{ type: "text", text: "ok" }] }, isError: false }, 3);

  const hint = tracker.snapshot({});
  // A well-formed pair is written as-is; any `\udXXX` escape would be a lone half.
  expect(hint).not.toMatch(/\\u[dD][89a-fA-F][0-9a-fA-F]{2}/);
  const nodes = JSON.parse(hint).nodes;
  // Start, update and end of the call still meet on one node.
  expect(nodes).toHaveLength(2);
  expect(nodes[0]).toMatchObject({ id: "call_\ufffdx", agent_type: "sc\ufffdout", status: "done" });
  expect(nodes[1]).toMatchObject({ id: "call_\ufffdx/0", parent_id: "call_\ufffdx", agent_type: "rev\ufffdewer" });
});

test("Pi activity adopts a tool first seen after a reload", async () => {
  const { createActivityTracker } = await importFresh("./pi/herdr-agent-state.ts");
  const tracker = createActivityTracker({});
  const call = { toolCallId: "call_2", toolName: "subagent", args: { agent: "scout", task: "Scan" } };
  tracker.update({ ...call, partialResult: { content: [{ type: "text", text: "(running...)" }], details: { mode: "single", results: [delegated("scout", "Scan")] } } }, 30);
  tracker.end({ ...call, result: { content: [{ type: "text", text: "Found it" }] }, isError: false }, 40);
  const nodes = JSON.parse(tracker.snapshot({ session_id: "s" })).nodes;
  // A single delegated agent is the tool node itself: no child row.
  expect(nodes).toHaveLength(1);
  expect(nodes[0]).toMatchObject({ id: "call_2", kind: "subagent", agent_type: "scout", status: "done", ended_at_ms: 40 });
  expect(nodes[0].started_at_ms).toBeUndefined();
});

test("Pi activity stays bounded", async () => {
  const { createActivityTracker } = await importFresh("./pi/herdr-agent-state.ts");
  const tracker = createActivityTracker({});
  const big = "x".repeat(5_000);
  for (let index = 0; index < 20; index += 1) {
    const call = { toolCallId: `call_${index}`, toolName: "subagent", args: { tasks: [] } };
    tracker.start(call, index);
    const results = Array.from({ length: 12 }, (_, step) =>
      delegated(`agent${step}`, `task ${step}`, { step: step + 1, messages: [assistant(`${big}${step}`)] }),
    );
    tracker.update({ ...call, partialResult: { content: [{ type: "text", text: big }], details: { mode: "chain", results } } }, index);
    if (index < 18) {
      tracker.end({ ...call, result: { content: [{ type: "text", text: "ok" }] }, isError: false }, index);
    }
  }
  const hint = tracker.snapshot({});
  expect(hint.length).toBeLessThanOrEqual(64 * 1024);
  const nodes = JSON.parse(hint).nodes as Record<string, any>[];
  const roots = nodes.filter((node) => node.parent_id === undefined);
  expect(roots).toHaveLength(8);
  // Running roots survive pruning; a long chain keeps its latest eight steps.
  expect(roots.map((node) => node.id)).toContain("call_18");
  expect(roots.map((node) => node.id)).toContain("call_19");
  expect(nodes.filter((node) => node.parent_id === "call_19").map((node) => node.id)).toEqual(
    Array.from({ length: 8 }, (_, step) => `call_19/${step + 4}`),
  );
  // Over budget, finished nodes give up their text first and keep the byte offset.
  const finished = nodes.find((node) => node.id === "call_17");
  expect(finished?.output.text).toBe("");
  expect(finished?.output.start).toBeGreaterThan(0);
});

test("Pi reports extension tool activity as snapshots from TUI sessions", async () => {
  const requests = await startRecordingServer("pi-activity");
  const { handlers, pi } = createExtensionHarness();
  (pi as Record<string, unknown>).getAllTools = toolCatalog({ bash: "builtin", subagent: "ext" });
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  const context = {
    ...piContext(() => false),
    sessionManager: {
      getSessionFile: () => "/tmp/pi-activity.jsonl",
      getSessionId: () => "pi-activity",
    },
  };
  await handlers.get("session_start")?.({ reason: "startup" }, context);
  await waitFor(() => requestStates(requests).length === 1);

  handlers.get("tool_execution_start")?.({ toolCallId: "b1", toolName: "bash", args: { command: "ls" } }, context);
  const call = { toolCallId: "s1", toolName: "subagent", args: { agent: "scout", task: "Scan the repo" } };
  handlers.get("tool_execution_start")?.(call, context);
  await waitFor(() => activityRequests(requests).length === 1);
  const [first] = activityRequests(requests);
  expect(first).toMatchObject({ pane_id: "test:p1", source: "herdr:pi", agent: "pi" });
  expect(typeof first.seq).toBe("number");
  const snapshot = JSON.parse(first.hint as string);
  expect(snapshot).toMatchObject({
    type: "herdr.activity.snapshot",
    version: 1,
    session_path: "/tmp/pi-activity.jsonl",
    session_id: "pi-activity",
  });
  expect(snapshot.nodes.map((node: { id: string }) => node.id)).toEqual(["s1"]);

  // Output-only updates are coalesced into one later snapshot.
  const update = (text: string) =>
    handlers.get("tool_execution_update")?.({ ...call, partialResult: { content: [{ type: "text", text }] } }, context);
  update("one");
  update("one two");
  update("one two three");
  await Bun.sleep(50);
  expect(activityRequests(requests)).toHaveLength(1);
  await waitFor(() => activityRequests(requests).length === 2, 2_000);
  expect(activityNodes(activityRequests(requests)[1])[0]).toMatchObject({
    summary: "one two three",
    output: { text: "one two three", start: 0 },
  });

  // Finishing is a tree change: sent at once, with a newer seq.
  handlers.get("tool_execution_end")?.({ ...call, result: { content: [{ type: "text", text: "done" }] }, isError: false }, context);
  await waitFor(() => activityRequests(requests).length === 3);
  const [, second, third] = activityRequests(requests);
  expect(third.seq as number).toBeGreaterThan(second.seq as number);
  expect(activityNodes(third)[0]).toMatchObject({ status: "done" });
});

test("Pi drops a pending activity snapshot when the session shuts down", async () => {
  const requests = await startRecordingServer("pi-activity-shutdown");
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  const context = piContext(() => false);
  await handlers.get("session_start")?.({ reason: "startup" }, context);
  const call = { toolCallId: "s2", toolName: "subagent", args: { agent: "scout", task: "Scan" } };
  handlers.get("tool_execution_start")?.(call, context);
  await waitFor(() => activityRequests(requests).length === 1);
  handlers.get("tool_execution_update")?.({ ...call, partialResult: { content: [{ type: "text", text: "partial" }] } }, context);
  handlers.get("session_shutdown")?.({}, context);
  await Bun.sleep(1_200);
  expect(activityRequests(requests)).toHaveLength(1);
  handlers.get("tool_execution_end")?.({ ...call, result: {}, isError: false }, context);
  await Bun.sleep(25);
  expect(activityRequests(requests)).toHaveLength(1);
});

async function startPiActivitySession(name: string, dropActivity: number, dropDelayMs = 0) {
  const requests = await startFlakyActivityServer(name, dropActivity, dropDelayMs);
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);
  const context = piContext(() => false);
  await handlers.get("session_start")?.({ reason: "startup" }, context);
  const call = { toolCallId: "r1", toolName: "subagent", args: { agent: "scout", task: "Scan" } };
  handlers.get("tool_execution_start")?.(call, context);
  return { requests, handlers, context, call };
}

test("Pi resends an undelivered activity snapshot once", async () => {
  // Both attempts of the first snapshot go unanswered.
  const { requests } = await startPiActivitySession("pi-activity-retry", 2);
  await waitFor(() => activityRequests(requests).length === 2);
  await waitFor(() => activityRequests(requests).length === 3, 3_500);
  const [first, second, retry] = activityRequests(requests);
  expect(second.hint).toBe(first.hint);
  expect(second.seq).toBe(first.seq);
  // The retry is the same snapshot under a newer `seq`.
  expect(retry.hint).toBe(first.hint);
  expect(retry.seq as number).toBeGreaterThan(first.seq as number);
  // Delivered: nothing more is sent.
  await Bun.sleep(2_300);
  expect(activityRequests(requests)).toHaveLength(3);
}, 10_000);

test("Pi retries an undelivered activity snapshot at most once", async () => {
  // The first snapshot and its retry both go unanswered.
  const { requests, handlers, context, call } = await startPiActivitySession("pi-activity-give-up", 4);
  await waitFor(() => activityRequests(requests).length === 4, 3_500);
  await Bun.sleep(2_300);
  expect(activityRequests(requests)).toHaveLength(4);
  expect(new Set(activityRequests(requests).map((params) => params.hint)).size).toBe(1);
  // The next change is still sent.
  handlers.get("tool_execution_end")?.({ ...call, result: {}, isError: false }, context);
  await waitFor(() => activityRequests(requests).length === 5);
  expect(activityNodes(activityRequests(requests)[4])[0]).toMatchObject({ id: "r1", status: "done" });
}, 10_000);

test("Pi skips the activity retry once a newer snapshot supersedes it", async () => {
  // A newer snapshot queued while the failing one is in flight supersedes it.
  const newer = await startPiActivitySession("pi-activity-superseded", 2);
  newer.handlers.get("tool_execution_end")?.({ ...newer.call, result: {}, isError: false }, newer.context);
  await waitFor(() => activityRequests(newer.requests).length === 3);
  await Bun.sleep(2_300);
  const sent = activityRequests(newer.requests);
  expect(sent).toHaveLength(3);
  expect(activityNodes(sent[2])[0]).toMatchObject({ status: "done" });
}, 10_000);

test("Pi drops a pending activity retry when the session shuts down", async () => {
  const { requests, handlers, context } = await startPiActivitySession("pi-activity-retry-shutdown", 2);
  await waitFor(() => activityRequests(requests).length === 2);
  handlers.get("session_shutdown")?.({}, context);
  await Bun.sleep(2_300);
  expect(activityRequests(requests)).toHaveLength(2);
}, 10_000);

test("Pi never retries a snapshot whose send fails after the session shut down", async () => {
  // Both attempts are held open for 300 ms, then closed unanswered.
  const { requests, handlers, context } = await startPiActivitySession("pi-activity-late-failure", 2, 300);
  await waitFor(() => activityRequests(requests).length === 1);
  // The first attempt is still in flight: it has not failed yet, so there is
  // no retry for shutdown to cancel.
  handlers.get("session_shutdown")?.({}, context);
  const shutdownAt = Date.now();
  expect(activityRequests(requests)).toHaveLength(1);
  // The send still makes its second attempt under the same `seq`; it fails
  // 300 ms later, about 0.6 s after shutdown. A retry would follow 2 s after
  // that, so none may arrive within 3.5 s of shutdown.
  await waitFor(() => activityRequests(requests).length === 2);
  await Bun.sleep(Math.max(0, shutdownAt + 3_500 - Date.now()));
  const sent = activityRequests(requests);
  expect(sent).toHaveLength(2);
  expect(sent[1].seq).toBe(sent[0].seq);
}, 10_000);

test("Pi still retries the next session's snapshots after a shutdown", async () => {
  const requests = await startFlakyActivityServer("pi-activity-next-session", 2);
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  const ended = createExtensionHarness();
  install(ended.pi);
  const context = piContext(() => false);
  await ended.handlers.get("session_start")?.({ reason: "startup" }, context);
  ended.handlers.get("session_shutdown")?.({}, context);

  // The replacing instance shares the module state with the ended one.
  const next = createExtensionHarness();
  install(next.pi);
  await next.handlers.get("session_start")?.({ reason: "new" }, context);
  next.handlers.get("tool_execution_start")?.(
    { toolCallId: "n1", toolName: "subagent", args: { agent: "scout", task: "Scan" } },
    context,
  );
  await waitFor(() => activityRequests(requests).length === 3, 3_500);
  const [first, , retry] = activityRequests(requests);
  expect(retry.hint).toBe(first.hint);
  expect(retry.seq as number).toBeGreaterThan(first.seq as number);
}, 10_000);

test("Pi never reports activity from headless modes", async () => {
  const requests = await startRecordingServer("pi-activity-rpc");
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  // A delegated `pi --mode json` child loads the same extension.
  const context = { ...piContext(() => false), hasUI: false, mode: "json" };
  await handlers.get("session_start")?.({ reason: "startup" }, context);
  const call = { toolCallId: "s3", toolName: "subagent", args: { agent: "scout", task: "Scan" } };
  handlers.get("tool_execution_start")?.(call, context);
  handlers.get("tool_execution_update")?.({ ...call, partialResult: { content: [{ type: "text", text: "x" }] } }, context);
  handlers.get("tool_execution_end")?.({ ...call, result: {}, isError: false }, context);
  await Bun.sleep(25);

  expect(requests).toEqual([]);
});

function completionHandlers(handlers: Map<string, Handler>): string[] {
  return ["agent_end", "agent_settled"].filter((event) => handlers.has(event));
}

function piContext(isIdle: () => boolean) {
  return {
    hasUI: true,
    mode: "tui",
    isIdle,
    sessionManager: {
      getSessionFile: () => undefined,
      getSessionId: () => undefined,
    },
  };
}

function requestStates(requests: unknown[]): unknown[] {
  return requests
    .filter((request) => isRecord(request) && request.method === "pane.report_agent")
    .map(requestState);
}

async function waitFor(predicate: () => boolean, timeoutMs = 1_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline && !predicate()) {
    await Bun.sleep(5);
  }
  expect(predicate()).toBe(true);
}

function requestState(request: unknown): unknown {
  if (!isRecord(request) || !isRecord(request.params)) {
    return undefined;
  }
  return request.params.state;
}

function requestSessionPath(request: unknown): unknown {
  if (!isRecord(request) || !isRecord(request.params)) {
    return undefined;
  }
  return request.params.agent_session_path;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}
