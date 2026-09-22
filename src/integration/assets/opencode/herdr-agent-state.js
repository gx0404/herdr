// installed by herdr
// managed by herdr; reinstalling or updating the integration overwrites this file.
// add custom hooks/plugins beside this file instead of editing it.
// HERDR_INTEGRATION_ID=opencode
// HERDR_INTEGRATION_VERSION=13

import net from "node:net";

const SOURCE = "herdr:opencode";
const AGENT = "opencode";
let reportSeq = Date.now() * 1000;
let activitySeq = Date.now() * 1000;
let requestChain = Promise.resolve();
let reportedRootSessionID;

// Track child sessions so their events cannot replace the pane's root session.
// User prompts carry the root id to preserve its identity and cross-talk guard.
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

export const HerdrAgentStatePlugin = async () => {
  if (
    process.env.HERDR_ENV !== "1" ||
    !process.env.HERDR_SOCKET_PATH ||
    !process.env.HERDR_PANE_ID
  ) {
    return {};
  }

  return {
    "chat.message": async ({ sessionID }) => {
      if (sessionID && childSessions.has(sessionID)) {
        return;
      }
      await reportState("working", sessionID);
    },
    event: async ({ event }) => {
      const type = event?.type;
      const properties = event?.properties ?? {};
      const sessionID = sessionIDFromProperties(properties);

      const info = properties.info;
      if (isChildInfo(info)) {
        childSessions.set(info.id, info.parentID);
      }
      if (ACTIVITY_EVENTS.has(type)) {
        await reportActivity(type);
      }
      if (sessionID && childSessions.has(sessionID)) {
        const state = CHILD_EVENT_STATES.get(type);
        if (state) {
          let rootSessionID = sessionID;
          while (childSessions.has(rootSessionID)) {
            rootSessionID = childSessions.get(rootSessionID);
          }
          await reportState(state, rootSessionID);
        }
        return;
      }

      switch (type) {
        case "session.created":
          // Creation is server-global, so an attached client may own it. The
          // TUI plugin separately reports the root selected in this pane.
          reportedRootSessionID = sessionID;
          break;
        case "session.updated":
          if (sessionID && sessionID !== reportedRootSessionID) {
            await reportSession(sessionID);
          }
          break;
        case "session.status": {
          const state = stateFromSessionStatus(properties.status);
          if (state) {
            await reportState(state, sessionID);
          } else {
            await reportSession(sessionID);
          }
          break;
        }
        case "tool.execute.before":
        case "tool.execute.after":
        case "permission.replied":
        case "question.replied":
        case "question.rejected":
        case "session.compacted":
          await reportState("working", sessionID);
          break;
        case "permission.asked":
        case "question.asked":
        case "session.error":
          await reportState("blocked", sessionID);
          break;
        case "session.idle":
          await reportState("idle", sessionID);
          break;
        case "session.deleted":
          break;
        default:
          break;
      }
    },
  };
};

// The loader takes `default` first: an object carrying `id`/`server`/`tui`
// only has its `server()` called, and named exports are ignored; a module
// without such a default falls back to every export. Keep both so either
// loader path reaches the same factory. V2 calls setup() instead. Its shared
// server cannot attribute sessions using its process environment: the
// pane-local TUI owns both selection and lifecycle reporting there, including
// remote servers.
export default {
  id: "herdr.opencode",
  server: HerdrAgentStatePlugin,
  setup() {},
};
