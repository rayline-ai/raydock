#!/usr/bin/env node
// Fixture test for rayline-router.ts: stock Pi, a synthetic Decisions service
// and two synthetic Anthropic Messages model servers, all on loopback.
// No credentials, no network, no real model.
//
// Usage: node examples/pi/test-router.mjs   (PI_BIN overrides the pi executable)

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const PI = process.env.PI_BIN ?? "pi";
const LOCAL = { action_id: "a".repeat(64), model: "qwen/qwen3.8-27b", worker: "qwen3.8-27b" };
const CLOUD = { action_id: "c".repeat(64), model: "anthropic/claude-opus-5.5", worker: "claude-opus-5.5" };
// What the public Routes API accepted on dev (2026-11-12); it refused `exclusive` and `value_usd`.
const ROUTING_MEMBERS = ["v_value", "temperature", "prices", "models", "excluded_models", "response_fields", "execution", "policy", "signals"];
const DONE = "Routed execution complete.";

const readBody = async (request) => {
	const chunks = [];
	for await (const chunk of request) chunks.push(chunk);
	return JSON.parse(Buffer.concat(chunks).toString("utf8"));
};

const listen = (handler) =>
	new Promise((resolve) => {
		const server = createServer((request, response) => {
			handler(request, response).catch((error) => response.writeHead(500).end(String(error)));
		});
		server.listen(0, "127.0.0.1", () => resolve({ server, url: `http://127.0.0.1:${server.address().port}` }));
	});

const hasToolResult = (body) => body.messages.some((message) => Array.isArray(message.content) && message.content.some((block) => block.type === "tool_result"));

/** One Anthropic Messages SSE response holding a single content block. */
function sse(response, model, block, stopReason) {
	const events = [
		["message_start", { message: { id: "msg_fixture", type: "message", role: "assistant", model, content: [], usage: { input_tokens: 10, output_tokens: 0 } } }],
		["content_block_start", { index: 0, content_block: block.type === "text" ? { type: "text", text: "" } : { ...block, input: {} } }],
		["content_block_delta", { index: 0, delta: block.type === "text" ? { type: "text_delta", text: block.text } : { type: "input_json_delta", partial_json: JSON.stringify(block.input) } }],
		["content_block_stop", { index: 0 }],
		["message_delta", { delta: { stop_reason: stopReason, stop_sequence: null }, usage: { output_tokens: 5 } }],
		["message_stop", {}],
	];
	response.writeHead(200, { "content-type": "text/event-stream" });
	for (const [type, data] of events) response.write(`event: ${type}\ndata: ${JSON.stringify({ type, ...data })}\n\n`);
	response.end();
}
const text = (response, model) => sse(response, model, { type: "text", text: DONE }, "end_turn");

/**
 * Servers, agent directory and router config for one scenario.
 * `decide(body)` returns a target, or `{ status }` to refuse; `local`/`cloud`
 * return "text", "tool" or an HTTP status to fail with.
 */
async function fixture({ mode = "current", config = {}, decide, local, cloud, reasoning = false, thinking = { mode: "off", budget_tokens: null } }) {
	const root = mkdtempSync(join(tmpdir(), "rayline-pi-router-"));
	const note = join(root, "note.txt");
	writeFileSync(note, "the fixture note\n");
	const decisions = [];
	const served = { local: [], cloud: [] };
	decide ??= (body) => (hasToolResult(body) ? CLOUD : LOCAL);
	local ??= (body) => (hasToolResult(body) ? "text" : "tool");
	cloud ??= () => "text";

	const decisionsServer = await listen(async (request, response) => {
		const body = await readBody(request);
		decisions.push({ body, headers: request.headers });
		const refuse = (status, code) => response.writeHead(status, { "content-type": "application/json" }).end(JSON.stringify({ error: { code } }));
		// Like the live cell, refuse routing members it does not know.
		if (Object.keys(body.routing).some((name) => !ROUTING_MEMBERS.includes(name))) return refuse(400, "routing_unknown_member");
		const pick = decide(body);
		if (pick.status) return refuse(pick.status, "fixture_refusal");
		response.writeHead(200, { "content-type": "application/json" });
		response.end(
			JSON.stringify({
				route_id: `rte_${decisions.length}`,
				model: pick.model,
				worker: pick.worker,
				thinking,
				usage: { encoded_input_tokens: 100, cache_read_tokens: 0 },
				latency_ms: 5,
				policy_action_id: pick.action_id,
				identity: { action_id: pick.action_id, policy_action_id: pick.action_id },
				routing: body.routing.execution ? { execution: { status: "applied", version: body.routing.execution.version, turn: body.routing.execution.turn } } : {},
			}),
		);
	});
	const model = (name, log, behave) =>
		listen(async (request, response) => {
			const body = await readBody(request);
			log.push(body);
			const reply = behave(body);
			if (reply === "text") text(response, name);
			else if (reply === "tool") sse(response, name, { type: "tool_use", id: "toolu_fixture", name: "read", input: { path: note } }, "tool_use");
			else response.writeHead(reply, { "content-type": "application/json" }).end(JSON.stringify({ type: "error", error: { type: "overloaded_error", message: "Overloaded" } }));
		});
	const localServer = await model("fixture-local", served.local, local);
	const cloudServer = await model("fixture-cloud", served.cloud, cloud);

	const provider = (baseUrl, id, price) => ({
		baseUrl,
		api: "anthropic-messages",
		apiKey: "synthetic-model-key",
		models: [{ id, name: id, reasoning, input: ["text"], cost: { input: price, output: price * 5, cacheRead: 0, cacheWrite: 0 }, contextWindow: 100000, maxTokens: 4096 }],
	});
	const agentDir = join(root, "agent");
	mkdirSync(agentDir, { recursive: true });
	writeFileSync(join(agentDir, "models.json"), JSON.stringify({ providers: { "fixture-local": provider(localServer.url, "qwen", 0), "fixture-cloud": provider(cloudServer.url, "opus", 5) } }));
	// The Rayline key is a Pi credential, not an environment variable.
	writeFileSync(join(agentDir, "auth.json"), JSON.stringify({ rayline: { type: "api_key", key: "synthetic-decisions-key" } }));
	writeFileSync(join(agentDir, "settings.json"), JSON.stringify({ retry: { baseDelayMs: 10 } }));
	const configPath = join(root, "router.json");
	writeFileSync(
		configPath,
		JSON.stringify({
			decisions: { url: `${decisionsServer.url}/v1/routes`, api_mode: mode, routing: mode === "current" ? {} : { policy: { preset: "arc-policy.hybrid-default.v1" } } },
			targets: [
				{ model: LOCAL.model, worker: LOCAL.worker, placement: "local", pi: "fixture-local/qwen", actions: [{ action_id: LOCAL.action_id, thinking_level: "off" }] },
				{ model: CLOUD.model, worker: CLOUD.worker, placement: "cloud", pi: "fixture-cloud/opus", actions: [{ action_id: CLOUD.action_id, thinking_level: "off" }] },
			],
			on_error: "fail",
			redecide_on_continuation: true,
			...config,
		}),
	);
	const servers = [decisionsServer, localServer, cloudServer];
	const children = [];
	const start = (extra, stdin) => {
		const args = ["-e", join(here, "rayline-router.ts"), "--model", "rayline/auto", "--no-session", "--offline", "--no-skills", "--no-prompt-templates", "--no-context-files", "--tools", "read", ...extra];
		const child = spawn(PI, args, { cwd: root, env: { ...process.env, PI_CODING_AGENT_DIR: agentDir, RAYLINE_PI_ROUTER_CONFIG: configPath }, stdio: [stdin, "pipe", "pipe"] });
		children.push(child);
		return child;
	};
	return {
		note,
		decisions,
		served,
		decisionsServer,
		/** `pi --print`: resolves with the exit code and combined output. */
		print: (prompt = `Read ${note} and report.`) =>
			new Promise((resolve, reject) => {
				const child = start(["--print", prompt], "ignore");
				let output = "";
				child.stdout.on("data", (chunk) => (output += chunk));
				child.stderr.on("data", (chunk) => (output += chunk));
				const timer = setTimeout(() => child.kill(), 60_000);
				child.on("error", reject);
				child.on("close", (code) => (clearTimeout(timer), resolve({ code, output })));
			}),
		/** `pi --mode rpc`: one process for several prompts. */
		rpc: (...args) => {
			const child = start(["--mode", "rpc", ...args], "pipe");
			const records = [];
			let buffer = "";
			child.stdout.on("data", (chunk) => {
				buffer += chunk;
				for (let end = buffer.indexOf("\n"); end >= 0; end = buffer.indexOf("\n")) {
					const line = buffer.slice(0, end).trim();
					buffer = buffer.slice(end + 1);
					if (line.startsWith("{")) records.push(JSON.parse(line));
				}
			});
			let sent = 0;
			return {
				records,
				prompt: (message) => child.stdin.write(`${JSON.stringify({ id: `req-${++sent}`, type: "prompt", message })}\n`),
				/** Waits until `find(records)` returns something. */
				until: async (what, find) => {
					for (const deadline = Date.now() + 30_000; Date.now() < deadline; await new Promise((wake) => setTimeout(wake, 25))) {
						const found = find(records);
						if (found) return found;
					}
					throw new Error(`timed out waiting for ${what}:\n${records.map((record) => JSON.stringify(record).slice(0, 300)).join("\n")}`);
				},
			};
		},
		close: () => {
			for (const child of children) child.kill();
			for (const { server } of servers) server.close(), server.closeAllConnections?.();
			rmSync(root, { recursive: true, force: true });
		},
	};
}

const scenarios = {
	async routed(mode) {
		const f = await fixture({ mode });
		try {
			const { code, output } = await f.print();
			assert.equal(code, 0, output);
			assert.match(output, /Routed execution complete\./, output);
			// Pi dispatched each turn to the model Decisions selected.
			assert.equal(f.served.local.length, 1, "local model requests");
			assert.equal(f.served.cloud.length, 1, "cloud model requests");
			assert.equal(f.served.local[0].model, "qwen");
			assert.equal(f.served.cloud[0].model, "opus");
			assert.ok(!("routing" in f.served.cloud[0]), "routing must not reach a model");
			assert.ok(!f.served.cloud[0].thinking || f.served.cloud[0].thinking.type === "disabled", "thinking off");
			// The cloud model received the local model's tool call and its result.
			const replay = JSON.stringify(f.served.cloud[0].messages);
			assert.ok(replay.includes("toolu_fixture") && replay.includes("the fixture note"), replay);

			assert.equal(f.decisions.length, 2, "one Decisions call per request");
			for (const { body, headers } of f.decisions) {
				assert.equal(headers.authorization, "Bearer synthetic-decisions-key");
				assert.equal(headers["x-rayline-wire-format"], "anthropic.messages.v1");
				assert.match(headers["x-conversation-id"], /^[0-9a-f]{64}$/);
				assert.equal(body.model, "arc-swift-1.0");
				assert.deepEqual(body.tools.map((tool) => tool.name), ["read"]);
				assert.ok(body.system?.length > 0, "system prompt is sent");
			}
			assert.equal(f.decisions[0].headers["x-conversation-id"], f.decisions[1].headers["x-conversation-id"]);
			const [first, second] = f.decisions.map(({ body }) => body.routing.execution);
			if (mode === "current") {
				assert.deepEqual(f.decisions[0].body.routing, { models: [LOCAL.model, CLOUD.model] });
			} else {
				assert.equal(first.version, "arc-execution-context.v2");
				assert.deepEqual(first.event, { kind: "task_start", basis: "explicit_client", compaction: null });
				assert.equal(first.last_served, null);
				assert.deepEqual(first.history, []);
				assert.deepEqual(first.targets.map((target) => [target.action_id, target.placement]), [[LOCAL.action_id, "local"], [CLOUD.action_id, "cloud"]]);
				assert.deepEqual(second.event, { kind: "tool_result", basis: "explicit_client", compaction: null });
				assert.deepEqual(second.last_served, { action_id: LOCAL.action_id, worker: LOCAL.worker, message: 1 });
				assert.deepEqual(second.history, [{ message: 1, action_id: LOCAL.action_id }]);
				assert.equal(second.turn.epoch, first.turn.epoch);
				assert.notEqual(second.turn.id, first.turn.id);
			}
			return "local turn -> fixture-local/qwen, tool-result turn -> fixture-cloud/opus";
		} finally {
			f.close();
		}
	},

	async sticky() {
		const f = await fixture({ config: { redecide_on_continuation: false } });
		try {
			const { code, output } = await f.print();
			assert.equal(code, 0, output);
			assert.equal(f.decisions.length, 1, "one Decisions call per user turn");
			assert.equal(f.served.local.length, 2, "local model requests");
			assert.equal(f.served.cloud.length, 0, "cloud model requests");
			return "one lookup, tool-result turn stays on fixture-local/qwen";
		} finally {
			f.close();
		}
	},

	async oversized() {
		const f = await fixture({ decide: () => ({ status: 413 }), config: { on_error: "previous", fallback: "fixture-local/qwen" } });
		try {
			const { code, output } = await f.print();
			assert.equal(code, 0, output);
			assert.match(output, /Routed execution complete\./, output);
			// Refused once, then not asked again for the same conversation.
			assert.equal(f.decisions.length, 1, "Decisions calls after a 413");
			assert.equal(f.served.local.length, 2, "fallback model requests");
			assert.equal(f.served.cloud.length, 0, "cloud model requests");
			return "413 once, then no lookups; both turns on the fallback";
		} finally {
			f.close();
		}
	},

	async overloaded() {
		// The local model is overloaded: Pi retries, and the retry asks again without it.
		const f = await fixture({ decide: (body) => (body.routing.models.includes(LOCAL.model) ? LOCAL : CLOUD), local: () => 529 });
		try {
			const { code, output } = await f.print();
			assert.equal(code, 0, output);
			assert.match(output, /Routed execution complete\./, output);
			assert.ok(f.served.local.length >= 1, "local model was tried");
			assert.equal(f.served.cloud.length, 1, "cloud model requests");
			assert.equal(f.decisions.length, 2, "Decisions calls");
			assert.deepEqual(f.decisions[0].body.routing.models, [LOCAL.model, CLOUD.model]);
			assert.deepEqual(f.decisions[1].body.routing.models, [CLOUD.model]);
			return "529 from fixture-local/qwen -> second lookup without it -> fixture-cloud/opus";
		} finally {
			f.close();
		}
	},

	async thinking() {
		const f = await fixture({ decide: () => CLOUD, reasoning: true, thinking: { mode: "on", budget_tokens: 8000 } });
		try {
			const { code, output } = await f.print();
			assert.equal(code, 0, output);
			const sent = f.served.cloud[0].thinking;
			assert.ok(sent && sent.type !== "disabled", `thinking reaches the model: ${JSON.stringify(sent)}`);
			return `thinking {on, 8000} -> model request thinking ${JSON.stringify(sent)}`;
		} finally {
			f.close();
		}
	},

	async unauthorized() {
		const f = await fixture({ decide: () => ({ status: 401 }), config: { on_error: "previous", fallback: "fixture-local/qwen" } });
		try {
			const { code, output } = await f.print();
			assert.notEqual(code, 0, "a refused key fails the request");
			assert.match(output, /HTTP 401/, output);
			assert.equal(f.served.local.length + f.served.cloud.length, 0, "no model request despite a fallback");
			return "401 fails the request; the fallback is not used";
		} finally {
			f.close();
		}
	},

	async offline() {
		// One process, two user turns: routed to cloud, then the router is gone.
		const f = await fixture({ decide: () => CLOUD, local: () => "text", config: { on_error: "previous", redecide_on_continuation: false } });
		try {
			const pi = f.rpc();
			const notice = (pattern) => (records) => records.find((record) => record.type === "extension_ui_request" && record.method === "notify" && pattern.test(record.message));
			pi.prompt("First question.");
			await pi.until("the cloud turn", () => f.served.cloud.length === 1 && pi.records.some((record) => record.type === "agent_end"));
			pi.prompt("/rayline");
			const before = await pi.until("/rayline output", notice(/^Rayline: /));
			assert.match(before.message, /1 lookups, 100 encoded input tokens billed, mean \d+ ms per lookup \(5 ms at the router\), 0 unrouted\. Last: rte_1 -> fixture-cloud\/opus in \d+ ms/);

			await new Promise((done) => (f.decisionsServer.server.close(done), f.decisionsServer.server.closeAllConnections()));
			pi.prompt("Second question.");
			await pi.until("the local turn", () => f.served.local.length === 1);
			assert.equal(f.served.cloud.length, 1, "the previous cloud model is not used offline");
			await pi.until("the unrouted warning", notice(/Rayline routing unavailable, using qwen/));
			pi.prompt("/rayline");
			const after = await pi.until("/rayline output", notice(/1 unrouted\. Last: unrouted -> fixture-local\/qwen/));
			assert.match(after.message, /^Rayline: 1 lookups/);
			return "/rayline reports lookups; router unreachable -> local target, not the previous cloud model";
		} finally {
			f.close();
		}
	},

	async timing() {
		// --rayline-timing: the footer shows each lookup's time, and says when no lookup was made.
		const f = await fixture({ decide: () => CLOUD, cloud: (body) => (hasToolResult(body) ? "text" : "tool"), config: { redecide_on_continuation: false } });
		try {
			const pi = f.rpc("--rayline-timing");
			const status = (pattern) => (records) =>
				records.find((record) => record.type === "extension_ui_request" && record.method === "setStatus" && pattern.test(record.statusText ?? ""));
			pi.prompt("First question.");
			const looked = await pi.until("the lookup time", status(/^route: opus in \d+ ms \(router 5 ms\)$/));
			await pi.until("the sticky continuation", status(/^route: kept opus, no lookup$/));
			return `footer: "${looked.statusText}", then "route: kept opus, no lookup"`;
		} finally {
			f.close();
		}
	},
};

const only = process.argv[2];
const plan = [["current", () => scenarios.routed("current")], ["execution_context_v2", () => scenarios.routed("execution_context_v2")], ...["sticky", "oversized", "overloaded", "thinking", "unauthorized", "offline", "timing"].map((name) => [name, scenarios[name]])];
for (const [name, run] of plan) if (!only || only === name) console.log(`ok ${name}: ${await run()}`);
