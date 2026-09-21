// installed by herdr
// managed by herdr; reinstalling or updating the integration overwrites this file.
// add custom hooks/plugins beside this file instead of editing it.
// HERDR_INTEGRATION_ID=pi
// HERDR_INTEGRATION_VERSION=10
// @ts-nocheck

import net from "node:net";
import path from "node:path";

const HERDR_ENV = process.env.HERDR_ENV;
const socketPath = process.env.HERDR_SOCKET_PATH;
const socketEndpoint =
  process.platform === "win32" && socketPath ? `\\\\.\\pipe\\${socketPath}` : socketPath;
const paneId = process.env.HERDR_PANE_ID;
const source = "herdr:pi";

function enabled() {
  return HERDR_ENV === "1" && !!socketPath && !!paneId;
}

function sendRequestAttempt(request: unknown, timeoutMs: number): Promise<boolean> {
  if (!enabled()) {
    return Promise.resolve(true);
  }

  return new Promise((resolve) => {
    let done = false;
    let timeout: ReturnType<typeof setTimeout> | undefined;
    const finish = (delivered: boolean) => {
      if (done) return;
      done = true;
      if (timeout) {
        clearTimeout(timeout);
      }
      socket.destroy();
      resolve(delivered);
    };

    const socket = net.createConnection(socketEndpoint!);
    socket.on("error", () => finish(false));
    socket.on("connect", () => socket.write(`${JSON.stringify(request)}\n`));
    socket.on("data", () => finish(true));
    socket.on("end", () => finish(false));
    timeout = setTimeout(() => finish(false), timeoutMs);
    timeout.unref?.();
  });
}

async function sendRequest(request: unknown): Promise<void> {
  if (await sendRequestAttempt(request, 500)) {
    return;
  }
  await sendRequestAttempt(request, 1500);
}

type AgentState = "working" | "blocked" | "idle";

type QueuedState = {
  state: AgentState;
  message?: string;
  seq: number;
};

let reportSeq = Date.now() * 1000;
let currentAgentSessionId: string | undefined;
let currentAgentSessionPath: string | undefined;

function nextReportSeq(): number {
  reportSeq += 1;
  return reportSeq;
}

function updateSessionRef(ctx: any): void {
  try {
    const file = ctx?.sessionManager?.getSessionFile?.();
    currentAgentSessionPath =
      typeof file === "string" &&
      (path.posix.isAbsolute(file) || path.win32.isAbsolute(file))
        ? file
        : undefined;
  } catch {
    currentAgentSessionPath = undefined;
  }

  try {
    const id = ctx?.sessionManager?.getSessionId?.();
    currentAgentSessionId = typeof id === "string" && id.length > 0 ? id : undefined;
  } catch {
    currentAgentSessionId = undefined;
  }
}

function withSessionRef(params: Record<string, unknown>): Record<string, unknown> {
  if (currentAgentSessionPath) {
    return { ...params, agent_session_path: currentAgentSessionPath };
  }
  if (currentAgentSessionId) {
    return { ...params, agent_session_id: currentAgentSessionId };
  }
  return params;
}

function currentSessionRef(): Record<string, unknown> | undefined {
  if (currentAgentSessionPath) {
    return { agent_session_path: currentAgentSessionPath };
  }
  if (currentAgentSessionId) {
    return { agent_session_id: currentAgentSessionId };
  }
  return undefined;
}

function reportSession(sessionStartSource?: string): Promise<void> {
  const sessionRef = currentSessionRef();
  if (!sessionRef) {
    return Promise.resolve();
  }

  return sendRequest({
    id: `${source}:session:${Date.now()}:${Math.random().toString(36).slice(2)}`,
    method: "pane.report_agent_session",
    params: {
      pane_id: paneId,
      source,
      agent: "pi",
      seq: nextReportSeq(),
      session_start_source: sessionStartSource,
      ...sessionRef,
    },
  });
}

function sendState(state: AgentState, message?: string, seq = nextReportSeq()): Promise<void> {
  return sendRequest({
    id: `${source}:${Date.now()}:${Math.random().toString(36).slice(2)}`,
    method: "pane.report_agent",
    params: withSessionRef({
      pane_id: paneId,
      source,
      agent: "pi",
      state,
      message,
      seq,
    }),
  });
}

// Usage push -----------------------------------------------------------------
//
// Pi's RPC mode starts a separate headless process, so it cannot observe the
// TUI session running in this pane. The extension reads the usage in-process
// instead and pushes it to Herdr with `account.usage.report`. These are session
// statistics for a multi-provider CLI, never an account quota: the payload
// carries the current provider/model so Herdr can link it to the subscription
// that actually pays for it.

const USAGE_PUSH_MIN_INTERVAL_MS = 5_000;

function finiteNumber(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

// `null` means "unknown", not zero: right after compaction Pi reports
// `tokens`/`percent` as null until the next LLM response.
function numberOrNull(value: unknown): number | null {
  return finiteNumber(value) ?? null;
}

// Session entries (and JSON mode events) carry `usage.cost` as an object
// (`{ input, output, cacheRead, cacheWrite, total }`); RPC `get_session_stats`
// reports `cost` as a plain number. Read each shape explicitly instead of
// coercing one into the other.
export function usageCostUsd(usage: any): number {
  const cost = usage?.cost;
  if (typeof cost === "number") {
    return finiteNumber(cost) ?? 0;
  }
  if (cost !== null && typeof cost === "object") {
    return finiteNumber(cost.total) ?? 0;
  }
  return 0;
}

// Summing per-entry costs in IEEE754 leaves tails: `0.36 + 0.04` is
// `0.39999999999999997`. Herdr computes this total itself (it is not a vendor
// decimal to preserve), and the account page renders `amount_decimal`
// verbatim, so round to 6 decimals — finer than any per-call price.
function roundCostUsd(cost: number): number {
  return Math.round(cost * 1e6) / 1e6;
}

function entryUsage(entry: any): any {
  if (entry?.type === "usage") {
    return entry.usage;
  }
  if (entry?.type === "branch_summary" || entry?.type === "compaction") {
    return entry.usage;
  }
  if (entry?.type !== "message") {
    return undefined;
  }
  const role = entry.message?.role;
  return role === "assistant" || role === "toolResult" ? entry.message.usage : undefined;
}

function nonEmptyString(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

// Builds the `official_payload` for `account.usage.report`, or `undefined`
// when the session has nothing to report yet. Totals mirror Pi's own
// `/session` statistics (every entry that carries usage).
export function collectUsagePayload(ctx: any): Record<string, unknown> | undefined {
  let contextUsage: any;
  try {
    contextUsage = ctx?.getContextUsage?.();
  } catch {
    contextUsage = undefined;
  }

  let entries: any[] = [];
  try {
    const listed = ctx?.sessionManager?.getEntries?.();
    entries = Array.isArray(listed) ? listed : [];
  } catch {
    entries = [];
  }

  const totals = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, cost: 0 };
  let usageEntries = 0;
  let provider: string | undefined;
  let model: string | undefined;
  for (const entry of entries) {
    const usage = entryUsage(entry);
    if (usage && typeof usage === "object") {
      usageEntries += 1;
      totals.input += finiteNumber(usage.input) ?? 0;
      totals.output += finiteNumber(usage.output) ?? 0;
      totals.cacheRead += finiteNumber(usage.cacheRead) ?? 0;
      totals.cacheWrite += finiteNumber(usage.cacheWrite) ?? 0;
      totals.cost += usageCostUsd(usage);
    }
    if (entry?.type === "message" && entry.message?.role === "assistant") {
      // The provider may answer with a different model than the requested
      // one; `responseModel` is what was actually billed.
      provider = nonEmptyString(entry.message.provider) ?? provider;
      model =
        nonEmptyString(entry.message.responseModel) ??
        nonEmptyString(entry.message.model) ??
        model;
    }
  }
  provider ??= nonEmptyString(ctx?.model?.provider);
  model ??= nonEmptyString(ctx?.model?.id);

  const hasContext = contextUsage !== null && typeof contextUsage === "object";
  if (!hasContext && usageEntries === 0) {
    return undefined;
  }

  const payload: Record<string, unknown> = { source, version: 1 };
  if (provider) {
    payload.provider = provider;
  }
  if (model) {
    payload.model = model;
  }
  if (hasContext) {
    payload.context = {
      tokens: numberOrNull(contextUsage.tokens),
      percent: numberOrNull(contextUsage.percent),
      context_window: numberOrNull(contextUsage.contextWindow),
    };
  }
  if (usageEntries > 0) {
    payload.tokens = {
      input: totals.input,
      output: totals.output,
      cache_read: totals.cacheRead,
      cache_write: totals.cacheWrite,
      total: totals.input + totals.output + totals.cacheRead + totals.cacheWrite,
    };
    payload.cost_usd = roundCostUsd(totals.cost);
  }
  return payload;
}

let lastUsagePushAt = 0;
let lastUsagePayload: string | undefined;

// `force` bypasses the throttle for boundaries that must not be dropped
// (settled, compaction, session start). Throttled turn pushes are superseded
// by the forced push when the agent settles.
function pushUsage(ctx: any, force: boolean): void {
  const payload = collectUsagePayload(ctx);
  if (!payload) {
    return;
  }
  const encoded = JSON.stringify(payload);
  if (encoded === lastUsagePayload) {
    return;
  }
  const now = Date.now();
  if (!force && now - lastUsagePushAt < USAGE_PUSH_MIN_INTERVAL_MS) {
    return;
  }
  lastUsagePushAt = now;
  lastUsagePayload = encoded;
  void sendRequest({
    id: `${source}:usage:${now}:${Math.random().toString(36).slice(2)}`,
    method: "account.usage.report",
    params: {
      agent: "pi",
      pane_id: paneId,
      official_payload: payload,
    },
  });
}

let sendInFlight = false;
let queuedState: QueuedState | undefined;

function queueState(state: AgentState, message?: string): void {
  queuedState = { state, message, seq: nextReportSeq() };
  if (!sendInFlight) {
    void drainStateQueue();
  }
}

async function drainStateQueue(): Promise<void> {
  if (sendInFlight) {
    return;
  }

  sendInFlight = true;
  try {
    while (queuedState) {
      const next = queuedState;
      queuedState = undefined;
      await sendState(next.state, next.message, next.seq);
    }
  } finally {
    sendInFlight = false;
    if (queuedState) {
      void drainStateQueue();
    }
  }
}

export default function (pi) {
  if (!enabled()) {
    return;
  }

  let agentActive = false;
  let blockedCount = 0;
  let blockedMessage: string | undefined;
  let lastState: AgentState | undefined;
  let lastMessage: string | undefined;
  let rootSession = false;

  function desiredState() {
    if (blockedCount > 0) {
      return { state: "blocked" as const, message: blockedMessage };
    }
    if (agentActive) {
      return { state: "working" as const, message: undefined };
    }
    return { state: "idle" as const, message: undefined };
  }

  function publishState(force = false) {
    const next = desiredState();
    if (!force && next.state === lastState && next.message === lastMessage) {
      return;
    }
    lastState = next.state;
    lastMessage = next.message;
    queueState(next.state, next.message);
  }

  pi.events.on("herdr:blocked", (data) => {
    if (!rootSession) {
      return;
    }
    if (!data?.active) {
      blockedCount = Math.max(0, blockedCount - 1);
      if (blockedCount === 0) {
        blockedMessage = undefined;
      }
      publishState();
      return;
    }

    blockedCount += 1;
    blockedMessage = data.label;
    publishState();
  });

  pi.on("session_start", async (event, ctx) => {
    // TUI only: RPC/JSON/print modes are headless (no PTY herdr can display),
    // and RPC still reports hasUI=true, so mode is the reliable gate.
    if (ctx?.mode !== "tui") {
      return;
    }
    rootSession = true;
    updateSessionRef(ctx);
    await reportSession(event?.reason);
    // A reload can replace this extension mid-run without emitting another agent_start.
    agentActive = ctx?.isIdle?.() === false;
    publishState(true);
    // A resumed session already has usage worth showing.
    pushUsage(ctx, true);
  });

  pi.on("agent_start", (_event, ctx) => {
    if (!rootSession) {
      return;
    }
    updateSessionRef(ctx);
    void reportSession();
    agentActive = true;
    publishState();
  });

  // Notification only: the handler returns nothing, so it never alters the
  // turn's proposed entries or continuation.
  pi.on("turn_end", (_event, ctx) => {
    if (!rootSession) {
      return;
    }
    pushUsage(ctx, false);
  });

  // Context usage is unknown (null) from here until the next LLM response.
  pi.on("session_compact", (_event, ctx) => {
    if (!rootSession) {
      return;
    }
    pushUsage(ctx, true);
  });

  pi.on("agent_settled", (_event, ctx) => {
    if (!rootSession || ctx?.isIdle?.() !== true) {
      return;
    }

    agentActive = false;
    publishState();
    pushUsage(ctx, true);
  });
}
