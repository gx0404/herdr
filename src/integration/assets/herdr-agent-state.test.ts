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
