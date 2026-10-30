#!/usr/bin/env node
"use strict";

const { loadPlacement, ConfigError } = require("./lib/placement");
const { spawn } = require("./lib/client");
const { listenHttp } = require("./lib/http");

process.on("uncaughtException", (err) => {
  console.error(err);
  process.exit(1);
});
process.on("unhandledRejection", (err) => {
  console.error(err);
  process.exit(1);
});

const placementPath = process.argv[2];
if (!placementPath) {
  console.error("usage: web-gateway <placement.json>");
  process.exit(1);
}

let place;
try {
  place = loadPlacement(placementPath);
} catch (e) {
  if (e instanceof ConfigError) {
    console.error(e.message);
    process.exit(1);
  }
  throw e;
}

const instances = [];
for (const inst of place.instances) {
  instances.push(spawn(place.core_socket, inst.instance_id, inst.agent_id, inst.author_id));
}

const gateway = { instances, conversations: place.conversations };
listenHttp({ host: place.host, port: place.port }, gateway).catch((err) => {
  console.error(err);
  process.exit(1);
});
