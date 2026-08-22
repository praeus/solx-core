#!/usr/bin/env node
// headersHelper for the "solx" MCP server in ../.mcp.json.
// Reads SOLX_SERVER_TOKEN from ../.env (never from the environment: Claude
// Code strips *TOKEN*/*SECRET*/*KEY*/etc vars before running project-scoped
// helpers) and prints the Authorization header as JSON on stdout.

"use strict";
const fs = require("fs");
const path = require("path");

const envPath = path.join(__dirname, "..", ".env");

let raw;
try {
  raw = fs.readFileSync(envPath, "utf8");
} catch (err) {
  process.stderr.write(`mcp-auth-headers: could not read ${envPath}: ${err.message}\n`);
  process.exit(1);
}

const match = raw.match(/^SOLX_SERVER_TOKEN=(.*)$/m);
if (!match || !match[1].trim()) {
  process.stderr.write(`mcp-auth-headers: SOLX_SERVER_TOKEN not set in ${envPath}\n`);
  process.exit(1);
}

const token = match[1].trim();
process.stdout.write(JSON.stringify({ Authorization: `Bearer ${token}` }));
