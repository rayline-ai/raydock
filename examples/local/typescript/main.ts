/**
 * Route an Anthropic SDK call to your on-device model via Rayline (TypeScript).
 *
 * Traffic flows through Rayline's local proxy on 127.0.0.1:20810 to the on-device
 * router (rld), which routes by your config — no special headers needed. The
 * default local config maps the model name "rayline-local" to your on-device
 * endpoint, so requesting that model lands the call on your local model.
 *
 * Start the router with `raydock router start` (no Claude session needed). It
 * defaults to routing every request through the router, so a plain call
 * requesting model "rayline-local" reaches the on-device model.
 *
 * Caveat: a raw API call only gets the model, not Claude Code's agent harness,
 * so the on-device model has no file tools — it answers prompts, it doesn't
 * browse files.
 *
 * Prerequisites:
 *   1. npm install
 *   2. Node 18+ (uses the built-in fetch + undici dispatcher).
 *   3. A running router with a local model loaded (give it a minute to load on
 *      first use):
 *          raydock router start
 *      Confirm it is up:
 *          curl -s http://127.0.0.1:20810/healthz   # expect "local_available": true
 *      Stop it later with: raydock router stop
 *   4. Because Rayline intercepts TLS, set CA_PATH to the proxy CA path for your
 *      OS:
 *          macOS:   ~/Library/Application Support/rayline/proxy-ca.pem
 *          Linux:   ~/.config/rayline/proxy-ca.pem
 *          Windows: %APPDATA%\rayline\proxy-ca.pem
 *
 * Run:
 *   npx tsx main.ts
 */

import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import Anthropic from "@anthropic-ai/sdk";
import { ProxyAgent, setGlobalDispatcher } from "undici";

// Rayline's local transparent proxy (override port with RAYLINE_PROXY_PORT).
const PROXY = "http://127.0.0.1:20810";

// Path to Rayline's proxy CA cert (see the comment block above for your OS).
const CA_PATH = join(
  homedir(),
  "Library/Application Support/rayline/proxy-ca.pem",
);

// Send all fetch traffic through the proxy and trust Rayline's CA for the
// intercepted TLS.
setGlobalDispatcher(
  new ProxyAgent({
    uri: PROXY,
    requestTls: { ca: readFileSync(CA_PATH) },
  }),
);

const client = new Anthropic({
  // Keep the real Anthropic base URL — the proxy intercepts this host.
  baseURL: "https://api.anthropic.com",
  // For requests Rayline routes to local, it injects the real credentials, so
  // this is just a non-empty placeholder the SDK requires.
  authToken: "rayline",
});

const message = await client.messages.create({
  // The default local config maps this model name to your on-device endpoint.
  model: "rayline-local",
  max_tokens: 256,
  messages: [
    {
      role: "user",
      content:
        "Use the Explore subagent to count the files in the current directory and tell me how many there are.",
    },
  ],
});

for (const block of message.content) {
  if (block.type === "text") {
    console.log(block.text);
  }
}
