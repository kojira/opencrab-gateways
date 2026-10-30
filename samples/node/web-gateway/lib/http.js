"use strict";

const http = require("node:http");
const { parseObject, dump, JsonSyntaxError } = require("./json");
const { parseUuid } = require("./wire");
const { connectionLive, postSaid, nextLive } = require("./client");

const JSON_TYPE = "application/json; charset=utf-8";

function jsonError(res, status, code, detail) {
  const body = dump({ error: { code, detail: detail === undefined ? null : detail } });
  res.writeHead(status, { "content-type": JSON_TYPE });
  res.end(body);
}

function jsonState(res, status, obj) {
  res.writeHead(status, { "content-type": JSON_TYPE });
  res.end(dump(obj));
}

function gone(res) {
  res.writeHead(404);
  res.end();
}

// D-1099: the gateway's own record says which instance owns a conversation (session key).
function findClient(gateway, session) {
  const owner = gateway.conversations.get(session);
  if (!owner) {
    return { none: true };
  }
  const client = gateway.instances.find((inst) => inst.instanceId === owner);
  if (!client) {
    return { unavailable: true };
  }
  return { client };
}

function parseAttachments(value) {
  if (!Array.isArray(value)) {
    return null;
  }
  const out = [];
  for (const item of value) {
    if (!item || typeof item !== "object" || Array.isArray(item)) {
      return null;
    }
    if (item.kind !== "image" || typeof item.url !== "string") {
      return null;
    }
    if (!item.url.startsWith("https://") || item.url.length <= "https://".length) {
      return null;
    }
    out.push({ kind: "image", url: item.url });
  }
  return out;
}

function parsePostBody(bytes) {
  let obj;
  try {
    obj = parseObject(bytes);
  } catch (e) {
    if (e instanceof JsonSyntaxError) {
      return null;
    }
    throw e;
  }
  const id = parseUuid(obj.client_message_id);
  if (!id) {
    return null;
  }
  if (typeof obj.text !== "string") {
    return null;
  }
  const attachments = parseAttachments(obj.attachments);
  if (!attachments) {
    return null;
  }
  if (obj.text.length === 0 && attachments.length === 0) {
    return null;
  }
  return { clientMessageId: id, text: obj.text, attachments };
}

function wireStatus(code) {
  switch (code) {
    case "bad_request":
      return 400;
    case "subject_unknown":
      return 404;
    case "instance_not_ready":
    case "instance_conflict":
    case "instance_active":
      return 409;
    case "store_error":
      return 500;
    default:
      return 502;
  }
}

function readBody(req) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    req.on("data", (c) => chunks.push(c));
    req.on("end", () => resolve(Buffer.concat(chunks)));
    req.on("error", reject);
  });
}

function sseWrite(res, event, dataObj) {
  res.write(`event: ${event}\ndata: ${dump(dataObj)}\n\n`);
}

function sseError(res, code) {
  res.writeHead(200, {
    "content-type": "text/event-stream",
    "cache-control": "no-cache",
    connection: "keep-alive",
  });
  sseWrite(res, "gate_error", { code, detail: null });
  res.end();
}

async function postMessage(gateway, sessionId, req, res) {
  const bytes = await readBody(req);
  const parsed = parsePostBody(bytes);
  if (!parsed) {
    jsonError(res, 400, "bad_request", null);
    return;
  }
  const origin = `web:${parsed.clientMessageId}`;
  const found = findClient(gateway, sessionId);
  if (found.none) {
    gone(res);
    return;
  }
  if (found.unavailable) {
    jsonError(res, 503, "instance_not_ready", null);
    return;
  }
  if (!connectionLive(found.client)) {
    jsonError(res, 503, "disconnect", null);
    return;
  }
  const outcome = await postSaid(found.client, sessionId, origin, parsed.text, parsed.attachments);
  if (outcome.refuse === "not_ready") {
    jsonError(res, 503, "instance_not_ready", null);
    return;
  }
  if (outcome.refuse === "busy") {
    jsonError(res, 409, "conversation_busy", null);
    return;
  }
  if (outcome.kind === "accepted") {
    jsonState(res, 202, {
      client_message_id: parsed.clientMessageId,
      origin,
      seq: { __int: true, lexeme: outcome.seq },
      state: "accepted",
    });
    return;
  }
  if (outcome.kind === "not_admitted") {
    jsonState(res, 403, { state: "not_admitted" });
    return;
  }
  if (outcome.kind === "wire_err") {
    jsonError(res, wireStatus(outcome.code), outcome.code, outcome.detail);
    return;
  }
  if (outcome.kind === "disconnected") {
    jsonError(res, 503, "disconnect", null);
    return;
  }
}

async function getEvents(gateway, sessionId, req, res) {
  const found = findClient(gateway, sessionId);
  if (found.none) {
    gone(res);
    return;
  }
  if (found.unavailable || !connectionLive(found.client)) {
    sseError(res, "disconnect");
    return;
  }
  res.writeHead(200, {
    "content-type": "text/event-stream",
    "cache-control": "no-cache",
    connection: "keep-alive",
  });
  res.flushHeaders();
  if (res.socket) {
    res.socket.setNoDelay(true);
  }
  const keep = setInterval(() => {
    res.write(": ping\n\n");
  }, 15_000);
  req.on("close", () => {
    clearInterval(keep);
  });
  const client = found.client;
  for (;;) {
    const ev = await nextLive(client, sessionId);
    if (!ev) {
      break;
    }
    if (ev.kind === "message") {
      sseWrite(res, "message", { text: ev.text });
    } else if (ev.kind === "activity") {
      sseWrite(res, "activity", { activity_id: ev.activityId, state: ev.state });
    } else if (ev.kind === "completed_no_reply") {
      sseWrite(res, "completed_no_reply", {});
    } else if (ev.kind === "error") {
      sseWrite(res, "gate_error", { code: ev.code, detail: ev.detail });
      break;
    }
  }
  clearInterval(keep);
  res.end();
}

function matchPath(pathname) {
  let m = pathname.match(/^\/api\/web-conversations\/([^/]+)\/messages$/);
  if (m) {
    return { kind: "messages", sessionId: decodeURIComponent(m[1]) };
  }
  m = pathname.match(/^\/api\/web-conversations\/([^/]+)\/events$/);
  if (m) {
    return { kind: "events", sessionId: decodeURIComponent(m[1]) };
  }
  m = pathname.match(/^\/rooms\/[^/]+\/messages$/);
  if (m) {
    return { kind: "gone" };
  }
  if (pathname === "/chat") {
    return { kind: "gone" };
  }
  return { kind: "missing" };
}

function listenHttp(bind, gateway) {
  const server = http.createServer((req, res) => {
    const url = new URL(req.url || "/", `http://${req.headers.host || "127.0.0.1"}`);
    const route = matchPath(url.pathname);
    const run = async () => {
      if (route.kind === "gone") {
        gone(res);
        return;
      }
      if (route.kind === "messages" && req.method === "POST") {
        await postMessage(gateway, route.sessionId, req, res);
        return;
      }
      if (route.kind === "events" && req.method === "GET") {
        await getEvents(gateway, route.sessionId, req, res);
        return;
      }
      gone(res);
    };
    run().catch((err) => {
      console.error(err);
      process.exit(1);
    });
  });
  return new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(bind.port, bind.host, () => {
      server.removeListener("error", reject);
      server.on("error", (err) => {
        console.error(err);
        process.exit(1);
      });
      resolve(server);
    });
  });
}

module.exports = { listenHttp };
