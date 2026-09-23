// installed by herdr
// managed by herdr; reinstalling or updating the integration overwrites this file.
// add custom hooks/plugins beside this file instead of editing it.
// HERDR_INTEGRATION_ID=pi
// HERDR_INTEGRATION_VERSION=10
// @ts-nocheck
//
// Reports to Herdr over the pane socket, from interactive (TUI) sessions only:
// - pane.report_agent / pane.report_agent_session: lifecycle and session identity
// - account.usage.report: session usage (tokens, cost, context window)
// - pane.report_agent_activity: extension tool activity, sent as a
//   `herdr.activity.snapshot` version 1 hint (see "Activity snapshot" below)

import { Buffer } from "node:buffer";
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

// Resolves to whether Herdr answered either attempt.
async function sendRequest(request: unknown): Promise<boolean> {
  if (await sendRequestAttempt(request, 500)) {
    return true;
  }
  return sendRequestAttempt(request, 1500);
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

// Activity snapshot ------------------------------------------------------------
//
// Pi has no built-in subagents, plans, todos or background jobs: extensions
// provide them. The official `subagent` example spawns
// `pi --mode json -p --no-session`, so delegated agents never write a session
// file Herdr could scan, and the session JSONL `id`/`parentId` tree is the
// conversation branch structure, not an agent tree. The activity tree is
// therefore observed from this process: every tool that is not one of Pi's
// built-in tools becomes a root node (id = toolCallId), and the official
// subagent `details.results[]` shape adds one child per delegated agent. The
// streamed update text of each node is kept as an append-only log whose head
// is dropped past a fixed tail, so Herdr can page it with byte cursors.
//
// The whole bounded tree is sent as a snapshot in the `hint` of
// `pane.report_agent_activity`; each snapshot replaces the previous one (at
// most one is in flight, so the latest sent is the latest received; `seq`
// still grows with every send). Format, version 1 (reader:
// `src/server/agent_activity/pi.rs`; later versions stay additive):
//
//   { "type": "herdr.activity.snapshot", "version": 1,
//     "session_path"?: string, "session_id"?: string,
//     "nodes": [{ "id", "parent_id"?, "kind", "label", "status",
//                 "agent_type"?, "summary"?, "started_at_ms"?, "ended_at_ms"?,
//                 "output"?: { "text", "start", "format" } }] }
//
// `kind` is `subagent` or `task`; `status` is `pending`, `running`, `done` or
// `failed`; nodes come in tree order (a parent precedes its children).
// `output.start` is the UTF-8 byte offset of `output.text` inside the node's
// log (bytes dropped from the head); `format` is `markdown` or `text`.

const ACTIVITY_HINT_TYPE = "herdr.activity.snapshot";
const ACTIVITY_HINT_VERSION = 1;
// Output-only updates are coalesced; lifecycle changes are sent at once.
const ACTIVITY_UPDATE_INTERVAL_MS = 1_000;
const ACTIVITY_MAX_ROOTS = 8;
const ACTIVITY_MAX_CHILDREN = 8;
const ACTIVITY_OUTPUT_TAIL_CHARS = 2_000;
const ACTIVITY_LABEL_CHARS = 120;
const ACTIVITY_SUMMARY_CHARS = 160;
const ACTIVITY_HINT_MAX_CHARS = 64 * 1024;
// A snapshot Herdr did not answer (both attempts failed, e.g. the server was
// restarting) is sent once more after this pause unless a newer one replaced it
// or its session shut down.
const ACTIVITY_RETRY_DELAY_MS = 2_000;
// Pi 0.87 built-in tool names (`allToolNames`). An extension that overrides
// one of them (a sandboxed `bash`) is still the same everyday tool, so these
// never become activity nodes.
const BUILTIN_TOOL_NAMES = new Set(["read", "bash", "powershell", "edit", "write", "grep", "find", "ls"]);

type ActivityStatus = "pending" | "running" | "done" | "failed";

type ActivityOutput = {
  log: string;
  start: number;
  last: string | undefined;
  format: "markdown" | "text";
};

type ActivityNode = {
  id: string;
  parentId?: string;
  kind: "subagent" | "task";
  label: string;
  status: ActivityStatus;
  agentType?: string;
  summary?: string;
  startedAt?: number;
  endedAt?: number;
  output: ActivityOutput;
  children: string[];
};

export type ActivitySession = { session_path?: string; session_id?: string };

// Bit flags returned by the tracker: what a tool event changed.
export const ACTIVITY_OUTPUT_CHANGED = 1;
export const ACTIVITY_TREE_CHANGED = 2;

const ANSI_SEQUENCE = /\u001b(?:\[[0-?]*[ -/]*[@-~]|\][^\u0007\u001b]*(?:\u0007|\u001b\\)?|[@-Z\\-_])/g;
const LINE_CONTROLS = /[\u0000-\u001f\u007f-\u009f]+/g;
const OUTPUT_CONTROLS = /[\u0000-\u0008\u000b-\u001f\u007f-\u009f]/g;

// Lone surrogates would serialize as `\udXXX` escapes that Herdr's JSON parser
// rejects, dropping the whole snapshot. Every string that reaches the snapshot
// goes through here (directly, or via `oneLine` / `cleanOutput`).
function wellFormed(text: string): string {
  return typeof text.toWellFormed === "function" ? text.toWellFormed() : text;
}

// The agent name of a delegated agent, shown next to its label.
function agentTypeOf(value: unknown): string | undefined {
  return oneLine(value, ACTIVITY_LABEL_CHARS);
}

function cleanOutput(text: string): string {
  return wellFormed(text)
    .replace(ANSI_SEQUENCE, "")
    .replace(/\r\n?/g, "\n")
    .replace(OUTPUT_CONTROLS, "");
}

function oneLine(text: unknown, max: number): string | undefined {
  if (typeof text !== "string") {
    return undefined;
  }
  const flat = wellFormed(text)
    .replace(ANSI_SEQUENCE, "")
    .replace(LINE_CONTROLS, " ")
    .replace(/\s+/g, " ")
    .trim();
  if (flat.length === 0) {
    return undefined;
  }
  if (flat.length <= max) {
    return flat;
  }
  let cut = max - 1;
  const code = flat.charCodeAt(cut - 1);
  if (code >= 0xd800 && code <= 0xdbff) {
    cut -= 1;
  }
  return `${flat.slice(0, cut)}…`;
}

function firstLine(text: string): string {
  for (const line of text.split("\n")) {
    if (line.trim().length > 0) {
      return line;
    }
  }
  return text;
}

function lastLine(text: string): string {
  let end = text.length;
  while (end > 0) {
    const start = text.lastIndexOf("\n", end - 1) + 1;
    const line = text.slice(start, end);
    if (line.trim().length > 0) {
      return line;
    }
    end = start - 1;
  }
  return text;
}

export function isExtensionTool(pi: any, toolName: unknown): boolean {
  if (typeof toolName !== "string" || toolName.length === 0 || BUILTIN_TOOL_NAMES.has(toolName)) {
    return false;
  }
  try {
    const tools = pi?.getAllTools?.();
    if (Array.isArray(tools)) {
      const info = tools.find((tool) => tool?.name === toolName);
      if (info) {
        return info.sourceInfo?.source !== "builtin";
      }
    }
  } catch {
    // Older Pi builds without tool metadata: the name check above decides.
  }
  return true;
}

function toolResultText(result: any): string | undefined {
  const content = result?.content;
  if (typeof content === "string") {
    return content;
  }
  if (!Array.isArray(content)) {
    return undefined;
  }
  const parts = content
    .filter((part) => part?.type === "text" && typeof part.text === "string")
    .map((part) => part.text);
  return parts.length > 0 ? parts.join("\n") : undefined;
}

function argsSummary(args: any): string | undefined {
  if (typeof args === "string") {
    return firstLine(args);
  }
  if (args === null || typeof args !== "object") {
    return undefined;
  }
  // The official subagent example: parallel `tasks` or sequential `chain`.
  if (Array.isArray(args.tasks) && args.tasks.length > 0) {
    return `parallel (${args.tasks.length} tasks)`;
  }
  if (Array.isArray(args.chain) && args.chain.length > 0) {
    return `chain (${args.chain.length} steps)`;
  }
  const agent = nonEmptyString(args.agent);
  for (const key of ["task", "prompt", "description", "query", "command", "title", "name", "path"]) {
    const value = nonEmptyString(args[key]);
    if (value) {
      return agent ? `${agent}: ${firstLine(value)}` : firstLine(value);
    }
  }
  if (agent) {
    return agent;
  }
  try {
    const encoded = JSON.stringify(args);
    return encoded === "{}" ? undefined : encoded;
  } catch {
    return undefined;
  }
}

function lastAssistantMessage(messages: unknown): any {
  if (!Array.isArray(messages)) {
    return undefined;
  }
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    if (messages[index]?.role === "assistant") {
      return messages[index];
    }
  }
  return undefined;
}

// Mirrors the example's `getFinalOutput`: the first text part of the latest
// assistant message that has one.
function delegatedFinalText(messages: unknown): string | undefined {
  if (!Array.isArray(messages)) {
    return undefined;
  }
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    const message = messages[index];
    if (message?.role !== "assistant" || !Array.isArray(message.content)) {
      continue;
    }
    for (const part of message.content) {
      if (part?.type === "text" && typeof part.text === "string") {
        return part.text;
      }
    }
  }
  return undefined;
}

// Status of one `details.results[]` entry. While the tool runs, `exitCode` is
// only meaningful as the parallel placeholder -1 (a streaming entry reports 0
// until its process exits), so the latest assistant `stopReason` decides.
function delegatedStatus(result: any, toolEnded: boolean): ActivityStatus {
  const stopReason = lastAssistantMessage(result.messages)?.stopReason ?? result.stopReason;
  if (stopReason === "error" || stopReason === "aborted") {
    return "failed";
  }
  const exitCode = finiteNumber(result.exitCode);
  const hasMessages = Array.isArray(result.messages) && result.messages.length > 0;
  if (exitCode === -1) {
    return hasMessages ? "running" : "pending";
  }
  if (exitCode !== undefined && exitCode !== 0) {
    return "failed";
  }
  if (toolEnded || stopReason === "stop" || stopReason === "length") {
    return "done";
  }
  return "running";
}

function delegatedOutput(result: any, status: ActivityStatus): string | undefined {
  const text = delegatedFinalText(result.messages);
  if (status !== "failed") {
    return text;
  }
  return nonEmptyString(result.errorMessage) ?? nonEmptyString(result.stderr) ?? text;
}

function delegatedLabel(result: any): string {
  const step = finiteNumber(result.step);
  const task = nonEmptyString(result.task);
  const head = step !== undefined ? `#${step} ${result.agent}` : result.agent;
  return oneLine(task ? `${head}: ${firstLine(task)}` : head, ACTIVITY_LABEL_CHARS) ?? "subagent";
}

function newOutput(format: ActivityOutput["format"]): ActivityOutput {
  return { log: "", start: 0, last: undefined, format };
}

// Appends the latest update text to the node's log: a growing text appends its
// new suffix, a replaced text is appended whole after a blank line. Returns
// whether the log changed.
function appendOutput(output: ActivityOutput, raw: unknown): boolean {
  if (typeof raw !== "string" || raw.length === 0) {
    return false;
  }
  const text = cleanOutput(raw);
  if (text.length === 0 || text === output.last) {
    return false;
  }
  const previous = output.last;
  const hasLog = output.log.length > 0 || output.start > 0;
  output.last = text;
  output.log +=
    previous !== undefined && text.startsWith(previous)
      ? text.slice(previous.length)
      : `${hasLog ? "\n\n" : ""}${text}`;
  if (output.log.length > ACTIVITY_OUTPUT_TAIL_CHARS) {
    let cut = output.log.length - ACTIVITY_OUTPUT_TAIL_CHARS;
    const code = output.log.charCodeAt(cut);
    if (code >= 0xdc00 && code <= 0xdfff) {
      cut += 1;
    }
    output.start += Buffer.byteLength(output.log.slice(0, cut), "utf8");
    output.log = output.log.slice(cut);
  }
  return true;
}

function isFinished(node: ActivityNode): boolean {
  return node.status === "done" || node.status === "failed";
}

// Tracks the extension tool executions of one extension instance. `now` is
// passed in so snapshots are reproducible in tests.
export function createActivityTracker(pi: any) {
  const nodes = new Map<string, ActivityNode>();
  const roots: string[] = [];

  function removeRoot(index: number) {
    const [id] = roots.splice(index, 1);
    for (const child of nodes.get(id)?.children ?? []) {
      nodes.delete(child);
    }
    nodes.delete(id);
  }

  // Finished roots go first; running ones only past a hard ceiling.
  function prune() {
    while (roots.length > ACTIVITY_MAX_ROOTS) {
      const finished = roots.findIndex((id) => {
        const node = nodes.get(id);
        return node !== undefined && isFinished(node);
      });
      if (finished >= 0) {
        removeRoot(finished);
      } else if (roots.length > ACTIVITY_MAX_ROOTS * 2) {
        removeRoot(0);
      } else {
        break;
      }
    }
  }

  // Also adopts tools first seen through an update or end event, e.g. after
  // `/reload` replaced this extension mid-run; their start time is unknown.
  function rootFor(
    event: any,
    startedAt: number | undefined,
  ): { node: ActivityNode; created: boolean } | undefined {
    // The id is replayed verbatim on every snapshot (and prefixes the child ids),
    // so it is made well-formed too; the mapping is deterministic, so start,
    // update and end events of one call still meet on the same node.
    const rawId = nonEmptyString(event?.toolCallId);
    const id = rawId === undefined ? undefined : wellFormed(rawId);
    const toolName = nonEmptyString(event?.toolName);
    if (!id || !toolName) {
      return undefined;
    }
    const existing = nodes.get(id);
    if (existing) {
      return existing.parentId === undefined ? { node: existing, created: false } : undefined;
    }
    if (!isExtensionTool(pi, toolName)) {
      return undefined;
    }
    const subagent = /agent/i.test(toolName);
    const node: ActivityNode = {
      id,
      kind: subagent ? "subagent" : "task",
      label: oneLine(`${toolName} ${argsSummary(event.args) ?? ""}`, ACTIVITY_LABEL_CHARS) ?? toolName,
      status: "running",
      agentType: agentTypeOf(event.args?.agent),
      startedAt,
      output: newOutput(subagent ? "markdown" : "text"),
      children: [],
    };
    nodes.set(id, node);
    roots.push(id);
    prune();
    return { node, created: true };
  }

  // The summary follows the newest line while the node runs and the headline
  // (first line) of its final text once it has finished.
  function recordText(node: ActivityNode, text: unknown): number {
    if (!appendOutput(node.output, text)) {
      return 0;
    }
    const latest = node.output.last ?? "";
    node.summary = oneLine(isFinished(node) ? firstLine(latest) : lastLine(latest), ACTIVITY_SUMMARY_CHARS);
    return ACTIVITY_OUTPUT_CHANGED;
  }

  function syncChildren(parent: ActivityNode, details: any, now: number, toolEnded: boolean): number {
    const results = details?.results;
    // A single delegated agent is the tool node itself.
    if (!Array.isArray(results) || details.mode === "single") {
      return 0;
    }
    let changed = 0;
    for (let index = Math.max(0, results.length - ACTIVITY_MAX_CHILDREN); index < results.length; index += 1) {
      const result = results[index];
      if (result === null || typeof result !== "object" || typeof result.agent !== "string") {
        continue;
      }
      const id = `${parent.id}/${index}`;
      const status = delegatedStatus(result, toolEnded);
      let child = nodes.get(id);
      if (!child) {
        child = {
          id,
          parentId: parent.id,
          kind: "subagent",
          label: delegatedLabel(result),
          status,
          agentType: agentTypeOf(result.agent),
          output: newOutput("markdown"),
          children: [],
        };
        nodes.set(id, child);
        parent.children.push(id);
        changed |= ACTIVITY_TREE_CHANGED;
      } else if (status !== child.status) {
        child.status = status;
        changed |= ACTIVITY_TREE_CHANGED;
      }
      // A queued (pending) agent has not started yet.
      if (status !== "pending") {
        child.startedAt ??= now;
      }
      if (isFinished(child)) {
        child.endedAt ??= now;
      }
      changed |= recordText(child, delegatedOutput(result, status));
    }
    // A long chain keeps only its latest steps.
    while (parent.children.length > ACTIVITY_MAX_CHILDREN) {
      nodes.delete(parent.children.shift()!);
      changed |= ACTIVITY_TREE_CHANGED;
    }
    return changed;
  }

  function orderedNodes(): ActivityNode[] {
    const ordered: ActivityNode[] = [];
    for (const id of roots) {
      const root = nodes.get(id);
      if (!root) {
        continue;
      }
      ordered.push(root);
      for (const child of root.children) {
        const node = nodes.get(child);
        if (node) {
          ordered.push(node);
        }
      }
    }
    return ordered;
  }

  function serializeNode(node: ActivityNode, withText: boolean): Record<string, unknown> {
    const encoded: Record<string, unknown> = {
      id: node.id,
      kind: node.kind,
      label: node.label,
      status: node.status,
    };
    if (node.parentId !== undefined) encoded.parent_id = node.parentId;
    if (node.agentType !== undefined) encoded.agent_type = node.agentType;
    if (node.summary !== undefined) encoded.summary = node.summary;
    if (node.startedAt !== undefined) encoded.started_at_ms = node.startedAt;
    if (node.endedAt !== undefined) encoded.ended_at_ms = node.endedAt;
    const output = node.output;
    if (output.log.length > 0 || output.start > 0) {
      encoded.output = withText
        ? { text: output.log, start: output.start, format: output.format }
        : { text: "", start: output.start + Buffer.byteLength(output.log, "utf8"), format: output.format };
    }
    return encoded;
  }

  return {
    start(event: any, now: number): number {
      return rootFor(event, now)?.created ? ACTIVITY_TREE_CHANGED : 0;
    },

    update(event: any, now: number): number {
      const root = rootFor(event, undefined);
      if (!root || isFinished(root.node)) {
        return 0;
      }
      let changed = root.created ? ACTIVITY_TREE_CHANGED : 0;
      changed |= recordText(root.node, toolResultText(event.partialResult));
      changed |= syncChildren(root.node, event.partialResult?.details, now, false);
      return changed;
    },

    end(event: any, now: number): number {
      const root = rootFor(event, undefined);
      if (!root || isFinished(root.node)) {
        return 0;
      }
      const node = root.node;
      // Pi 0.87 sets `isError` only when `execute` throws; the subagent example
      // reports failures as `{ isError: true }` inside the returned result.
      node.status = event.isError === true || event.result?.isError === true ? "failed" : "done";
      node.endedAt = now;
      recordText(node, toolResultText(event.result));
      syncChildren(node, event.result?.details, now, true);
      for (const id of node.children) {
        const child = nodes.get(id);
        if (child && !isFinished(child)) {
          child.status = node.status;
          child.endedAt ??= now;
        }
      }
      prune();
      return ACTIVITY_TREE_CHANGED;
    },

    // Drops the tree (a new, resumed or forked session). Returns whether there
    // was anything to clear.
    reset(): boolean {
      const hadNodes = roots.length > 0;
      nodes.clear();
      roots.length = 0;
      return hadNodes;
    },

    // The `herdr.activity.snapshot` hint. Output text is dropped first from
    // finished nodes, then from all nodes, if the snapshot would exceed the
    // size budget (labels and summaries alone stay far below it).
    snapshot(session: ActivitySession): string {
      const ordered = orderedNodes();
      const encode = (withText: (node: ActivityNode) => boolean) =>
        JSON.stringify({
          type: ACTIVITY_HINT_TYPE,
          version: ACTIVITY_HINT_VERSION,
          ...session,
          nodes: ordered.map((node) => serializeNode(node, withText(node))),
        });
      let encoded = encode(() => true);
      if (encoded.length > ACTIVITY_HINT_MAX_CHARS) {
        encoded = encode((node) => !isFinished(node));
      }
      if (encoded.length > ACTIVITY_HINT_MAX_CHARS) {
        encoded = encode(() => false);
      }
      return encoded;
    },
  };
}

// Only the snapshot's copy of the session reference is made well-formed; the
// lifecycle reports keep Pi's value, which `pi --session` resumes.
function activitySession(): ActivitySession {
  const session: ActivitySession = {};
  if (currentAgentSessionPath) {
    session.session_path = wellFormed(currentAgentSessionPath);
  }
  if (currentAgentSessionId) {
    session.session_id = wellFormed(currentAgentSessionId);
  }
  return session;
}

type QueuedActivity = {
  hint: string;
  seq: number;
  // This send is already the one retry of an undelivered snapshot.
  retry: boolean;
  // `activityEpoch` when it was queued.
  epoch: number;
};

let activityInFlight = false;
let queuedActivity: QueuedActivity | undefined;
let lastActivityHint: string | undefined;
let activityRetryTimer: ReturnType<typeof setTimeout> | undefined;
// Advances when a session shuts down: a snapshot queued before that belongs to
// the ended session and is never retried.
let activityEpoch = 0;

// Latest snapshot wins: at most one request in flight, identical snapshots are
// not sent twice.
function queueActivity(hint: string, retry = false): void {
  if (hint === lastActivityHint) {
    return;
  }
  cancelActivityRetry();
  lastActivityHint = hint;
  queuedActivity = { hint, seq: nextReportSeq(), retry, epoch: activityEpoch };
  if (!activityInFlight) {
    void drainActivityQueue();
  }
}

function cancelActivityRetry(): void {
  if (activityRetryTimer) {
    clearTimeout(activityRetryTimer);
    activityRetryTimer = undefined;
  }
}

// The last snapshot never reached Herdr. Without this its tree would stay stale
// (a finished tool still "running") until the next change, and an identical
// later snapshot would be deduplicated away. A newer queued snapshot already
// supersedes it; otherwise it becomes sendable again and is retried once, with
// a fresh `seq`, unless its session has shut down since it was queued (shutdown
// cancels a scheduled retry, but a send still in flight fails only afterwards).
// Nothing is written to the console: it is Pi's TUI.
function activityUndelivered(failed: QueuedActivity): void {
  if (queuedActivity || lastActivityHint !== failed.hint) {
    return;
  }
  lastActivityHint = undefined;
  if (failed.retry || failed.epoch !== activityEpoch) {
    return;
  }
  activityRetryTimer = setTimeout(() => {
    activityRetryTimer = undefined;
    // Shutdown also cancels this timer; the epoch check keeps the ended
    // session's snapshot from being resent should the timer fire anyway.
    if (failed.epoch === activityEpoch) {
      queueActivity(failed.hint, true);
    }
  }, ACTIVITY_RETRY_DELAY_MS);
  activityRetryTimer.unref?.();
}

async function drainActivityQueue(): Promise<void> {
  if (activityInFlight) {
    return;
  }

  activityInFlight = true;
  try {
    while (queuedActivity) {
      const next = queuedActivity;
      queuedActivity = undefined;
      const delivered = await sendRequest({
        id: `${source}:activity:${Date.now()}:${Math.random().toString(36).slice(2)}`,
        method: "pane.report_agent_activity",
        params: {
          pane_id: paneId,
          source,
          agent: "pi",
          hint: next.hint,
          seq: next.seq,
        },
      });
      if (!delivered) {
        activityUndelivered(next);
      }
    }
  } finally {
    activityInFlight = false;
    if (queuedActivity) {
      void drainActivityQueue();
    }
  }
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

  const activity = createActivityTracker(pi);
  let activityTimer: ReturnType<typeof setTimeout> | undefined;
  let lastActivityFlushAt = 0;

  function cancelActivityTimer() {
    if (activityTimer) {
      clearTimeout(activityTimer);
      activityTimer = undefined;
    }
  }

  function flushActivity() {
    cancelActivityTimer();
    lastActivityFlushAt = Date.now();
    queueActivity(activity.snapshot(activitySession()));
  }

  // Tree changes (a tool or delegated agent starts, finishes or fails) go out
  // at once; streamed output is coalesced to one snapshot per interval.
  function publishActivity(changed: number) {
    if (changed & ACTIVITY_TREE_CHANGED) {
      flushActivity();
      return;
    }
    if (!(changed & ACTIVITY_OUTPUT_CHANGED) || activityTimer) {
      return;
    }
    const wait = Math.max(0, lastActivityFlushAt + ACTIVITY_UPDATE_INTERVAL_MS - Date.now());
    activityTimer = setTimeout(flushActivity, wait);
    activityTimer.unref?.();
  }

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
    // A replaced session starts with an empty activity tree.
    if (activity.reset()) {
      flushActivity();
    }
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

  // Notification only, and never awaited on the socket: Pi awaits extension
  // handlers inside the tool loop.
  pi.on("tool_execution_start", (event) => {
    if (!rootSession) {
      return;
    }
    publishActivity(activity.start(event, Date.now()));
  });

  pi.on("tool_execution_update", (event) => {
    if (!rootSession) {
      return;
    }
    publishActivity(activity.update(event, Date.now()));
  });

  pi.on("tool_execution_end", (event) => {
    if (!rootSession) {
      return;
    }
    publishActivity(activity.end(event, Date.now()));
  });

  // This instance is being replaced (session switch, fork, reload): a pending
  // snapshot must not be sent under the next session's identity, and the ended
  // session's snapshots are no longer retried. Cancelling the retry timer covers
  // a send that already failed; advancing the epoch covers one still in flight.
  pi.on("session_shutdown", () => {
    cancelActivityTimer();
    cancelActivityRetry();
    activityEpoch += 1;
    rootSession = false;
  });
}
