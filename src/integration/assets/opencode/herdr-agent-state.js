// installed by herdr
// managed by herdr; reinstalling or updating the integration overwrites this file.
// add custom hooks/plugins beside this file instead of editing it.
// HERDR_INTEGRATION_ID=opencode
// HERDR_INTEGRATION_VERSION=14

import net from "node:net";

const SOURCE = "herdr:opencode";
const AGENT = "opencode";
let reportSeq = Date.now() * 1000;
let activitySeq = Date.now() * 1000;
let attentionSeq = Date.now() * 1000;
let requestChain = Promise.resolve();
let reportedRootSessionID;

// Track child sessions so their events cannot replace the pane's root session.
// Root status reports carry the root id to preserve its identity and
// cross-talk guard; child prompts that block report against their root.
// A session counts as a child when its parentID is a non-empty string naming a
// different session. OpenCode maps rows as `parentID: parent_id ?? undefined`,
// so a root's own events carry the key with no value and stay roots; requiring
// the parent to be a session this process already saw would instead demote a
// real child to a root whenever its parent's events arrived elsewhere first,
// letting that child's id overwrite the pane's agent session.
const childSessions = new Map();
const CHILD_EVENT_STATES = new Map([
  ["permission.asked", "blocked"],
  ["question.asked", "blocked"],
  ["permission.replied", "working"],
  ["question.replied", "working"],
  ["question.rejected", "working"],
]);

// Events that can change the session tree or todos. The server refreshes the
// activity tree from opencode.db on each hint; the hint only names the event.
const ACTIVITY_EVENTS = new Set([
  "session.created",
  "session.updated",
  "session.status",
  "session.idle",
  "session.deleted",
  "todo.updated",
]);

function nextReportSeq() {
  reportSeq += 1;
  return reportSeq;
}

function nextActivitySeq() {
  activitySeq += 1;
  return activitySeq;
}

function nextAttentionSeq() {
  attentionSeq += 1;
  return attentionSeq;
}

// One line of at most `max` UTF-16 units: Herdr rejects an attention title over
// 200 and a detail over 2000 characters.
function clip(value, max) {
  if (typeof value !== "string") {
    return undefined;
  }
  const text = value.replace(/\s+/g, " ").trim();
  if (!text) {
    return undefined;
  }
  if (text.length <= max) {
    return text;
  }
  let cut = max - 1;
  const code = text.charCodeAt(cut - 1);
  if (code >= 0xd800 && code <= 0xdbff) {
    cut -= 1;
  }
  return `${text.slice(0, cut)}\u2026`;
}

function sessionIDFromProperties(properties) {
  return typeof properties?.sessionID === "string" && properties.sessionID
    ? properties.sessionID
    : undefined;
}

function isChildInfo(info) {
  return (
    typeof info?.id === "string" &&
    info.id !== "" &&
    typeof info.parentID === "string" &&
    info.parentID !== "" &&
    info.parentID !== info.id
  );
}

const SESSION_STATE_BY_STATUS = new Map([
  ["idle", "idle"],
  ["active", "working"],
  ["busy", "working"],
  ["pending", "working"],
  ["retry", "working"],
  ["running", "working"],
  ["streaming", "working"],
  ["working", "working"],
]);

function stateFromSessionStatus(status) {
  const kind = typeof status === "string" ? status : status?.type;
  return typeof kind === "string"
    ? SESSION_STATE_BY_STATUS.get(kind.toLowerCase())
    : undefined;
}

// `params` may be a function so that its content is decided at dispatch time.
function request(method, params) {
  const pending = requestChain.then(() => requestOnce(method, params));
  requestChain = pending.catch(() => {});
  return pending;
}

function requestOnce(method, params) {
  const paneId = process.env.HERDR_PANE_ID;
  const socketPath = process.env.HERDR_SOCKET_PATH;

  if (!paneId || !socketPath) {
    return Promise.resolve();
  }

  const socketEndpoint =
    process.platform === "win32" ? `\\\\.\\pipe\\${socketPath}` : socketPath;

  const requestId = `${SOURCE}:${Date.now()}:${Math.floor(Math.random() * 1_000_000)
    .toString()
    .padStart(6, "0")}`;
  // Activity hints bring their own sequence so lifecycle numbering stays
  // contiguous for the server's stale-report check.
  const resolved = typeof params === "function" ? params() : params;
  const seq = resolved.seq ?? nextReportSeq();
  const request = {
    id: requestId,
    method,
    params: {
      pane_id: paneId,
      source: SOURCE,
      agent: AGENT,
      ...resolved,
      seq,
    },
  };

  return new Promise((resolve) => {
    const client = net.createConnection(socketEndpoint, () => {
      client.write(`${JSON.stringify(request)}\n`);
    });

    const finish = () => {
      client.destroy();
      resolve();
    };

    client.setTimeout(500, finish);
    client.on("data", finish);
    client.on("error", finish);
    client.on("end", finish);
    client.on("close", resolve);
  });
}

function reportSession(sessionID) {
  if (!sessionID) {
    return Promise.resolve();
  }
  return request("pane.report_agent_session", { agent_session_id: sessionID });
}

function reportState(state, sessionID) {
  const params = { state };
  if (sessionID) {
    reportedRootSessionID = sessionID;
    params.agent_session_id = sessionID;
  }
  return request("pane.report_agent", params);
}

// `session.error` carries `{ name, data: { message } }`. A failed turn latches in
// Herdr until the user looks at the pane or the agent works again. Failures
// bring their own sequence so lifecycle numbering stays contiguous.
function reportFailure(error) {
  const params = { kind: "failed", seq: nextAttentionSeq() };
  const title = clip(error?.name, 200);
  const detail = clip(error?.data?.message, 2000);
  if (title) params.title = title;
  if (detail) params.detail = detail;
  return request("pane.report_agent_attention", params);
}

// The error of each root session's turn, held until that turn ends. OpenCode's
// session processor publishes `session.error` and then sets the session idle
// (`session.status` idle, then `session.idle`): nothing waits for the user, so
// the error changes no state, and the failure goes out just before the idle
// that ends the turn. A context overflow that OpenCode compacts publishes the
// error and keeps working; the next working report drops the failure.
const pendingFailures = new Map();

function settleFailure(sessionID, state, reports) {
  if (!sessionID || !pendingFailures.has(sessionID)) {
    return;
  }
  if (state === "idle") {
    reports.push(reportFailure(pendingFailures.get(sessionID)));
  }
  if (state === "idle" || state === "working") {
    pendingFailures.delete(sessionID);
  }
}

// One slot: events arriving before the queued report is dispatched only
// replace its hint, so a burst collapses into a single request. Rate limiting
// of the actual refresh belongs to the server.
let queuedActivityHint;

function reportActivity(hint) {
  if (queuedActivityHint !== undefined) {
    queuedActivityHint = hint;
    return Promise.resolve();
  }
  queuedActivityHint = hint;
  return request("pane.report_agent_activity", () => {
    const params = { hint: queuedActivityHint, seq: nextActivitySeq() };
    queuedActivityHint = undefined;
    return params;
  });
}

// Stops at the first repeated session so a parent cycle in bad session data
// cannot spin forever inside OpenCode's event dispatch.
function rootSessionOf(sessionID) {
  const seen = new Set();
  let rootSessionID = sessionID;
  while (childSessions.has(rootSessionID) && !seen.has(rootSessionID)) {
    seen.add(rootSessionID);
    rootSessionID = childSessions.get(rootSessionID);
  }
  return rootSessionID;
}

// Every report an event causes is queued before this returns, so reports
// leave in event order. OpenCode delivers events without awaiting handlers,
// and a turn ends in one burst: the last loop step publishes busy, then the
// runner publishes idle and session.idle. Awaiting anything before queueing
// the state report let that busy report overtake the idle ones and strand the
// pane on working.
function handleEvent(event) {
  const type = event?.type;
  const properties = event?.properties ?? {};
  const sessionID = sessionIDFromProperties(properties);
  const reports = [];

  const info = properties.info;
  if (isChildInfo(info)) {
    childSessions.set(info.id, info.parentID);
  }
  if (ACTIVITY_EVENTS.has(type)) {
    reports.push(reportActivity(type));
  }
  if (sessionID && childSessions.has(sessionID)) {
    const state = CHILD_EVENT_STATES.get(type);
    if (state) {
      reports.push(reportState(state, rootSessionOf(sessionID)));
    }
    return settle(reports);
  }

  switch (type) {
    case "session.created":
      // Creation is server-global, so an attached client may own it. The
      // TUI plugin separately reports the root selected in this pane.
      reportedRootSessionID = sessionID;
      break;
    case "session.updated":
      if (sessionID && sessionID !== reportedRootSessionID) {
        reports.push(reportSession(sessionID));
      }
      break;
    case "session.status": {
      // A turn starts `working` only here: OpenCode's session runner sets busy
      // whenever a turn actually runs and idle when it ends. The replies below
      // only resume a turn that is already running.
      const state = stateFromSessionStatus(properties.status);
      if (state) settleFailure(sessionID, state, reports);
      reports.push(state ? reportState(state, sessionID) : reportSession(sessionID));
      break;
    }
    case "tool.execute.before":
    case "tool.execute.after":
    case "permission.replied":
    case "question.replied":
    case "question.rejected":
    case "session.compacted":
      settleFailure(sessionID, "working", reports);
      reports.push(reportState("working", sessionID));
      break;
    case "permission.asked":
    case "question.asked":
      reports.push(reportState("blocked", sessionID));
      break;
    case "session.error":
      // An abort is the user stopping the turn, not a failure. An error without
      // a session ends no turn this plugin follows; it is reported at once.
      if (properties.error?.name === "MessageAbortedError") break;
      if (sessionID) {
        pendingFailures.set(sessionID, properties.error);
      } else {
        reports.push(reportFailure(properties.error));
      }
      break;
    case "session.idle":
      // The failure goes first: Herdr announces a turn that ends after a
      // failure report once, as a failure.
      settleFailure(sessionID, "idle", reports);
      reports.push(reportState("idle", sessionID));
      break;
    case "session.deleted":
      pendingFailures.delete(sessionID);
      break;
    default:
      break;
  }
  return settle(reports);
}

function settle(reports) {
  return Promise.all(reports).then(() => undefined);
}

// Processes that do not own lifecycle still hint the activity tree: a hint only
// names the event, and the server re-reads the tree from opencode.db.
function handleActivityEvent(event) {
  const type = event?.type;
  return settle(ACTIVITY_EVENTS.has(type) ? [reportActivity(type)] : []);
}

function ownsLocalLifecycle() {
  const args = process.argv.slice(2);
  const separator = args.indexOf("--");
  if (separator !== -1) args.splice(separator);
  if (args.some((arg) => arg === "--attach" || arg.startsWith("--attach="))) return false;
  while (args[0] === "--print-logs" || args[0] === "--log-level" || args[0]?.startsWith("--log-level=")) {
    args.splice(0, args[0] === "--log-level" ? 2 : 1);
  }
  // These local clients have no TUI plugin. Shared servers and the TUI worker
  // cannot identify their attached panes; their lifecycle belongs to each TUI.
  return args[0] === "run" ||
    (!["serve", "web", "attach"].includes(args[0]) && args.includes("--mini"));
}

export const HerdrAgentStatePlugin = async () => {
  if (
    process.env.HERDR_ENV !== "1" ||
    !process.env.HERDR_SOCKET_PATH ||
    !process.env.HERDR_PANE_ID
  ) {
    return {};
  }
  // Full TUIs report lifecycle from the pane-local TUI plugin; this server
  // hook only keeps the activity-tree hints there.
  if (!ownsLocalLifecycle()) {
    return {
      event: async ({ event }) => handleActivityEvent(event),
    };
  }

  // No `chat.message` hook: OpenCode runs it for every message it stores,
  // including ones that never start a turn. `session.prompt` with
  // `noReply: true` (plugins use it to leave a note for the next turn) stores
  // the message and returns without running the loop, so no idle would follow
  // a `working` reported there.
  //
  // `async` keeps failures off OpenCode's dispatch loop, which calls every
  // plugin's hook without awaiting it; the body still queues its reports
  // before the first await, so event order is preserved.
  return {
    event: async ({ event }) => handleEvent(event),
  };
};

// The loader takes `default` first: an object carrying `id`/`server`/`tui`
// only has its `server()` called, and named exports are ignored; a module
// without such a default falls back to every export. Keep both so either
// loader path reaches the same factory. V1 local run/Mini retain their server
// lifecycle hooks. V1/V2 full TUIs own both selection and lifecycle, including
// when attached to a shared remote server; V2 calls setup() instead.
export default {
  id: "herdr.opencode",
  server: HerdrAgentStatePlugin,
  setup() {},
};
