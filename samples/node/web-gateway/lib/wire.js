"use strict";

const { invariant } = require("./fail");
const { parseObject, dump, isInt, intLexeme, JsonSyntaxError } = require("./json");

const MAX_FRAME = 1_048_576;
const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

function parseRequestId(raw) {
  if (typeof raw !== "string") {
    return null;
  }
  const n = Buffer.byteLength(raw, "utf8");
  if (n < 1 || n > 128) {
    return null;
  }
  return raw;
}

function parseUuid(raw) {
  if (typeof raw !== "string" || !UUID_RE.test(raw)) {
    return null;
  }
  return raw;
}

function requireStr(obj, key) {
  const v = obj[key];
  return typeof v === "string" ? v : null;
}

function nonemptyStr(obj, key) {
  const v = requireStr(obj, key);
  return v && v.length > 0 ? v : null;
}

function optId(obj) {
  return parseRequestId(requireStr(obj, "id"));
}

// D-1099: hello names the instance and its agent; core finds or creates the instance.
function helloFrame(id, instanceId, agentId) {
  return {
    id,
    m: "hello",
    protocol: 3,
    operation_protocol: 1,
    final_delivery: "automatic",
    operations: [],
    instance_id: instanceId,
    agent_id: agentId,
  };
}

// D-1099: a said names the gateway's session key; there is no binding.
function saidFrame(id, session, origin, authorId, text, attachments) {
  return {
    id,
    m: "said",
    session,
    origin,
    author_id: authorId,
    text,
    attachments: attachments.map((a) => ({ kind: a.kind, url: a.url })),
  };
}

function okFrame(id) {
  return { id, m: "ok" };
}

function errFrame(id, code, detail) {
  return { id, m: "err", code, detail: detail === undefined ? null : detail };
}

function sayText(payload) {
  if (!payload || typeof payload !== "object" || Array.isArray(payload)) {
    return null;
  }
  const t = payload.text;
  return typeof t === "string" && t.length > 0 ? t : null;
}

function encodeFrame(value) {
  const bytes = Buffer.from(`${dump(value)}\n`, "utf8");
  if (bytes.length > MAX_FRAME) {
    return null;
  }
  return bytes;
}

function parseFrameBytes(bytes) {
  invariant(Buffer.isBuffer(bytes), "frame bytes");
  let raw = bytes;
  if (raw.length > 0 && raw[raw.length - 1] === 0x0a) {
    raw = raw.subarray(0, raw.length - 1);
  }
  let obj;
  try {
    obj = parseObject(raw);
  } catch (e) {
    if (e instanceof JsonSyntaxError) {
      return { type: "frame_error", code: "bad_request" };
    }
    throw e;
  }
  return parseCoreMsg(obj);
}

function parseCoreMsg(obj) {
  const m = requireStr(obj, "m");
  if (m === null) {
    return { type: "invalid", id: optId(obj), code: "bad_request", m: "" };
  }
  if (m === "say") {
    const parsed = parseSay(obj);
    return parsed || { type: "invalid", id: optId(obj), code: "bad_request", m };
  }
  if (m === "activity") {
    const parsed = parseActivity(obj);
    return parsed || { type: "invalid", id: optId(obj), code: "bad_request", m };
  }
  if (m === "ok" || m === "err") {
    const parsed = parseResponse(obj, m);
    return parsed || { type: "invalid", id: optId(obj), code: "response_invalid", m };
  }
  if (m === "hello" || m === "said") {
    return { type: "reverse", id: optId(obj), m };
  }
  return { type: "unknown", id: optId(obj), m };
}

function parseSay(obj) {
  const id = parseRequestId(requireStr(obj, "id"));
  const session = nonemptyStr(obj, "session");
  const payload = obj.payload;
  if (!id || !session || !payload || typeof payload !== "object" || Array.isArray(payload) || isInt(payload)) {
    return null;
  }
  return { type: "say", id, session, payload };
}

function parseActivity(obj) {
  const session = nonemptyStr(obj, "session");
  const activityId = parseUuid(requireStr(obj, "activity_id"));
  const state = nonemptyStr(obj, "state");
  if (!session || !activityId || (state !== "started" && state !== "ended")) {
    return null;
  }
  return { type: "activity", session, activityId, state };
}

function parseResponse(obj, m) {
  const id = parseRequestId(requireStr(obj, "id"));
  if (!id) {
    return null;
  }
  if (m === "ok") {
    if (!Object.prototype.hasOwnProperty.call(obj, "seq")) {
      return { type: "response", id, ok: true, seq: { present: false } };
    }
    if (obj.seq === null) {
      return { type: "response", id, ok: true, seq: { present: true, value: null } };
    }
    if (!isInt(obj.seq)) {
      return null;
    }
    return { type: "response", id, ok: true, seq: { present: true, value: intLexeme(obj.seq) } };
  }
  const code = requireStr(obj, "code");
  if (code === null) {
    return null;
  }
  if (!Object.prototype.hasOwnProperty.call(obj, "detail")) {
    return null;
  }
  const detail = obj.detail;
  if (detail !== null && typeof detail !== "string") {
    return null;
  }
  return { type: "response", id, ok: false, seq: { present: false }, code, detail };
}

module.exports = {
  MAX_FRAME,
  parseRequestId,
  parseUuid,
  helloFrame,
  saidFrame,
  okFrame,
  errFrame,
  sayText,
  encodeFrame,
  parseFrameBytes,
};
