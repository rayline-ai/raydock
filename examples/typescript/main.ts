/**
 * Use Rayline's hosted cloud router from TypeScript via the Anthropic SDK.
 *
 * This points the official `@anthropic-ai/sdk` client at the hosted Rayline
 * router instead of api.anthropic.com. The router decides where each request
 * goes and forwards it on with your provider credentials.
 *
 * Prerequisites:
 *   1. npm install
 *   2. Generate a router key at https://platform.rayline.ai/keys. It looks like
 *      "rlk-...". Export it as RAYLINE_API_KEY.
 *
 * Run:
 *   npx tsx main.ts
 */

import Anthropic from "@anthropic-ai/sdk";

// Hosted Rayline router endpoint.
const BASE_URL = "https://api.rayline.ai";

// Your Rayline router key (rlk-...). Router keys are *bearer* tokens, so they
// are passed via `authToken` (Authorization: Bearer ...), not `apiKey`.
const API_KEY = process.env.RAYLINE_API_KEY;
if (!API_KEY) {
  console.error("RAYLINE_API_KEY is not set. Export your Rayline router key (rlk-...) as RAYLINE_API_KEY.");
  process.exit(1);
}

const client = new Anthropic({ baseURL: BASE_URL, authToken: API_KEY });

const message = await client.messages.create({
  // "arc-sage-1.0" lets ARC pick the concrete model.
  model: "arc-sage-1.0",
  max_tokens: 2048,
  messages: [{ role: "user", content: "In one sentence, what is Rayline?" }],
});

for (const block of message.content) {
  if (block.type === "text") {
    console.log(block.text);
  }
}
