
// Subagent (child) sessions must not speak for the pane: their created or
// updated reports would replace the resumable root session, and their idle
// would mark the pane idle while the root is still working. Only a child's
// prompts for the user (blocked) and the replies to them (working) are
// forwarded, attributed to the root session.
// Retire ancestry on session.deleted. Do not evict live ancestry by age or
// size: a later prompt from that child would otherwise claim the pane as a
// root session. A hard cap needs an authoritative ancestry lookup first.
const childSessions = new Map();
const CHILD_EVENT_STATES = new Map([
  ["permission.asked", STATE.blocked],
  ["question.asked", STATE.blocked],
  ["permission.replied", STATE.working],
  ["question.replied", STATE.working],
  ["question.rejected", STATE.working],
]);

function sessionIDFromProperties(properties) {
  if (typeof properties?.sessionID === "string" && properties.sessionID) {
    return properties.sessionID;
  }
  return typeof properties?.info?.id === "string" && properties.info.id
    ? properties.info.id
    : undefined;
}

// A session payload names its parent when it is a subagent's.
function trackChildSession(info) {
  if (typeof info?.id === "string" && info.id && typeof info.parentID === "string" && info.parentID) {
    childSessions.set(info.id, info.parentID);
  }
}

function rootSessionOf(sessionID) {
  let rootSessionID = sessionID;
  const seen = new Set();
  while (childSessions.has(rootSessionID) && !seen.has(rootSessionID)) {
    seen.add(rootSessionID);
    rootSessionID = childSessions.get(rootSessionID);
  }
  return rootSessionID;
}

const SESSION_STATE_BY_STATUS = new Map([
  ["idle", STATE.idle],
  ["active", STATE.working],
  ["busy", STATE.working],
  ["pending", STATE.working],
  ["retry", STATE.working],
  ["running", STATE.working],
  ["streaming", STATE.working],
  ["working", STATE.working],
]);

// OpenCode and Kilo's local-run checks consume the same leading CLI options.
// Their commands have different ownership rules, so each decoder applies its
// own verdict after this shared normalization.
function localLifecycleArgs() {
  const args = process.argv.slice(2);
  const separator = args.indexOf("--");
  if (separator !== -1) args.splice(separator);
  if (args.some((arg) => arg === "--attach" || arg.startsWith("--attach="))) {
    return undefined;
  }
  while (
    args[0] === "--print-logs" ||
    args[0] === "--log-level" ||
    args[0]?.startsWith("--log-level=")
  ) {
    args.splice(0, args[0] === "--log-level" ? 2 : 1);
  }
  return args;
}

// Status arrives either as a bare string or as an object such as
// `{ type: "busy" }` / `{ type: "retry", ... }`.
function stateFromSessionStatus(status) {
  const kind = typeof status === "string" ? status : status?.type;
  return typeof kind === "string"
    ? SESSION_STATE_BY_STATUS.get(kind.toLowerCase())
    : undefined;
}
