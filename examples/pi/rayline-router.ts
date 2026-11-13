/**
 * Rayline router for Pi (prototype): a virtual model whose router is ARC.
 *
 * Registers `rayline/arc`. Before every request Pi calls `route()`, which asks
 * the Rayline Routes API (`POST /v1/routes`) which model should serve the turn
 * and returns that model to Pi. Pi dispatches the request itself, so the
 * footer, `/session` costs, context limits and compaction all follow the model
 * that actually ran. `raydock hybrid` does both steps in its Go core; here the
 * core is not in the path.
 *
 * Configuration is a JSON file named by RAYLINE_PI_ROUTER_CONFIG (see
 * cloud.json and local.json, and README.md). `targets` binds what Decisions can
 * select to models in Pi's catalog. The Rayline key is the `rayline` provider's
 * credential in Pi's auth.json; it is never read from the environment, which
 * Pi's bash tool inherits.
 *
 * When it asks: on user turns, after a compaction, and on a retry after an
 * overloaded provider (with that model left out). Tool continuations stay on
 * the model that answered last unless `redecide_on_continuation` is set, and
 * Pi's own direct requests (compaction summaries) never ask.
 *
 * Two API modes, as in the core:
 * - `current` (default): the conversation plus `routing.models`, which makes
 *   the targets the only candidates. Every candidate is treated alike, so a
 *   model served locally is never preferred for being local.
 * - `execution_context_v2`: adds `routing.execution` (event, last_served,
 *   history, targets with placement, and a size bound for local targets),
 *   which the `arc-policy.prefer-local.v1` policy needs to prefer a local
 *   model. Needs each target's `actions` (ARC action id and the Pi thinking
 *   level it stands for).
 *
 * Not done here, and done by the Go core: tokenizer capacity facts, cache
 * evidence, re-consults, private append, thinking controls beyond Pi's levels,
 * and receipt/price verification.
 *
 * Usage: RAYLINE_PI_ROUTER_CONFIG=router.json pi -e ./rayline-router.ts --model rayline/arc
 * Add `--rayline-timing` (or `"show_decision_time": true`) to show each
 * lookup's total time in the footer, next to the model Pi already shows.
 */

import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import type { Message } from "@earendil-works/pi-ai";
import type { ExtensionAPI, ExtensionContext, ModelRoute, ModelRouteRequest } from "@earendil-works/pi-coding-agent";

const CONFIG_ENV = "RAYLINE_PI_ROUTER_CONFIG";
const TIMING_FLAG = "rayline-timing";
/** The longest preamble a served chat template adds, as the core assumes. */
const TEMPLATE_MARGIN_TOKENS = 512;
const V2 = "execution_context_v2";
const CONTEXT_VERSION = "arc-execution-context.v2";
const WIRE_FORMAT = "anthropic.messages.v1";
/** Start of the user message Pi writes in place of compacted history. */
const COMPACTION_PREFIX = "The conversation history before this point was compacted into the following summary:";
const LEVELS = ["off", "minimal", "low", "medium", "high", "xhigh", "max"] as const;
type Level = (typeof LEVELS)[number];

interface TargetAction {
	action_id: string;
	thinking_level: Level;
}

interface Target {
	/** Model name Decisions reports, e.g. `z-ai/glm-5.3-flash`. */
	model: string;
	worker: string;
	placement: "local" | "cloud";
	/** Pi catalog model, `provider/id`. */
	pi: string;
	/** v2 only: one entry per bound ARC action. */
	actions?: TargetAction[];
	/** USD per million tokens the user pays, sent as `routing.prices` when set. */
	prices?: Prices;
	/** The model's window: sizes local prompts (v2), and sets a `rayline/` gateway model's window. Default Pi's contextWindow for `pi`. */
	context_window?: number;
}

interface Prices {
	input: number;
	cache_read: number;
	cache_write: number;
	output: number;
}

interface Config {
	decisions: {
		url: string;
		model?: string;
		api_mode?: "current" | typeof V2;
		result?: "top" | "swift" | "sage" | "custom";
		cell_tag?: string;
		/** Sent as configured: value_usd, temperature, signals, policy... */
		routing?: Record<string, unknown>;
		timeout_ms?: number;
	};
	targets: Target[];
	/** When Decisions is unavailable: keep the model that answered last, or fail the request. */
	on_error?: "previous" | "fail";
	/** Pi model (`provider/id`) used when Decisions is unavailable and nothing has answered yet. */
	fallback?: string;
	/** Ask again on every tool continuation. Off by default: each call adds latency and a billed lookup. */
	redecide_on_continuation?: boolean;
	/** Show each lookup's time in the footer. Same as the `--rayline-timing` flag. */
	show_decision_time?: boolean;
	context_window?: number;
	max_tokens?: number;
}

/** Stored by Pi on the session branch. `head` identifies the current prefix. */
interface RouterState {
	head: string;
	/** Decisions refused this prefix as too large; do not ask again until it changes. */
	oversized?: boolean;
}

/** Decisions answered with an HTTP error. */
class Refused extends Error {
	constructor(
		readonly status: number,
		code: unknown,
	) {
		super(`Decisions request refused (HTTP ${status}${typeof code === "string" ? `, ${code}` : ""})`);
	}
	/** A bad key or a malformed request: falling back would hide it. */
	get fatal() {
		return this.status >= 400 && this.status < 500 && ![402, 408, 413, 429].includes(this.status);
	}
}

const PROVIDER = "rayline";
/** Errors that mean this machine cannot reach the network, as opposed to a slow or failing router. */
const OFFLINE = new Set(["ECONNREFUSED", "ENOTFOUND", "EAI_AGAIN", "ENETUNREACH", "ENETDOWN", "EHOSTUNREACH"]);
function isOffline(error: unknown) {
	const cause = (error as { cause?: { code?: string; errors?: { code?: string }[] } } | undefined)?.cause;
	return [cause?.code, ...(cause?.errors ?? []).map((entry) => entry.code)].some((code) => code && OFFLINE.has(code));
}
const OVERLOADED = /\b(429|5\d\d)\b|overload|rate.?limit|capacity|unavailable/i;

/** Pi level for ARC's `thinking`. The budget thresholds are a placeholder, not a contract. */
function levelOf(thinking: { mode?: string; budget_tokens?: number | null } | undefined): Level | undefined {
	if (!thinking?.mode) return undefined;
	if (thinking.mode === "off") return "off";
	// The live API answers `on` with no budget: it leaves the amount to the caller.
	if (thinking.budget_tokens == null) return undefined;
	const budget = thinking.budget_tokens;
	if (budget <= 1024) return "minimal";
	if (budget <= 4096) return "low";
	if (budget <= 16384) return "medium";
	return budget <= 32768 ? "high" : "xhigh";
}

type Request = ModelRouteRequest<RouterState>;
type Route = ModelRoute<RouterState>;
type WireBlock = Record<string, unknown>;
interface WireMessage {
	role: "user" | "assistant";
	content: WireBlock[];
}

const sha256 = (text: string) => createHash("sha256").update(text).digest("hex");

function loadConfig(): Config {
	const path = process.env[CONFIG_ENV];
	if (!path) throw new Error(`${CONFIG_ENV} must name the router configuration file`);
	const config = JSON.parse(readFileSync(path, "utf8")) as Config;
	if (!config.decisions?.url || !config.targets?.length) {
		throw new Error(`${path}: decisions.url and targets are required`);
	}
	if (config.decisions.api_mode === V2 && config.targets.some((target) => !target.actions?.length)) {
		throw new Error(`${path}: ${V2} requires actions on every target`);
	}
	return config;
}

function textOf(content: string | readonly { type: string; text?: string }[]): string {
	if (typeof content === "string") return content;
	return content.flatMap((block) => (block.type === "text" && block.text ? [block.text] : [])).join("\n");
}

function userBlocks(content: string | readonly { type: string; text?: string; data?: string; mimeType?: string }[]) {
	if (typeof content === "string") return [{ type: "text", text: content }];
	return content.map((block): WireBlock =>
		block.type === "image"
			? { type: "image", source: { type: "base64", media_type: block.mimeType, data: block.data } }
			: { type: "text", text: block.text ?? "" },
	);
}

/**
 * The conversation as an Anthropic Messages body, which is what Decisions
 * reads. `assistants` maps each wire assistant message to the Pi message it
 * came from, so history can name wire indexes. Thinking blocks are left out:
 * their signatures belong to whichever model produced them.
 */
function toWire(messages: readonly Message[]) {
	const system: string[] = [];
	const wire: WireMessage[] = [];
	const assistants = new Map<number, Extract<Message, { role: "assistant" }>>();
	const push = (role: WireMessage["role"], blocks: WireBlock[]) => {
		if (blocks.length === 0) return;
		const last = wire.at(-1);
		if (role === "user" && last?.role === "user") last.content.push(...blocks);
		else wire.push({ role, content: blocks });
	};
	for (const message of messages) {
		if (message.role === "system") {
			// Pi carries its own prompt as named sections; a forced prompt is plain content.
			const sections = (message as { sections?: Record<string, string | null> }).sections ?? {};
			system.push(textOf(message.content), ...Object.values(sections).filter((section) => section !== null));
		} else if (message.role === "user") {
			push("user", userBlocks(message.content));
		} else if (message.role === "toolResult") {
			push("user", [
				{
					type: "tool_result",
					tool_use_id: message.toolCallId,
					is_error: message.isError,
					content: userBlocks(message.content),
				},
			]);
		} else {
			const blocks = message.content.flatMap((block): WireBlock[] => {
				if (block.type === "text") return block.text ? [{ type: "text", text: block.text }] : [];
				if (block.type === "toolCall") {
					return [{ type: "tool_use", id: block.id, name: block.name, input: block.arguments }];
				}
				return [];
			});
			push("assistant", blocks);
			if (blocks.length > 0) assistants.set(wire.length - 1, message);
		}
	}
	return { system: system.filter(Boolean).join("\n\n"), wire, assistants };
}

function activeTools(pi: ExtensionAPI): WireBlock[] {
	const active = new Set(pi.getActiveTools());
	return pi
		.getAllTools()
		.filter((tool) => active.has(tool.name))
		.map((tool) => ({ name: tool.name, description: tool.description, input_schema: tool.parameters }));
}

class Router {
	private readonly byPi = new Map<string, Target>();
	private readonly byAction = new Map<string, { target: Target; action: TargetAction }>();

	constructor(
		private readonly config: Config,
		private readonly pi: ExtensionAPI,
	) {
		for (const target of config.targets) {
			this.byPi.set(target.pi, target);
			for (const action of target.actions ?? []) this.byAction.set(action.action_id, { target, action });
		}
	}

	private get v2() {
		return this.config.decisions.api_mode === V2;
	}

	/** The ARC action a Pi model and thinking level stand for, if it is a bound target. */
	private actionOf(model: { provider: string; id: string } | undefined, level: string | undefined) {
		const target = model && this.byPi.get(`${model.provider}/${model.id}`);
		const action = target?.actions?.find((entry) => entry.thinking_level === level) ?? target?.actions?.[0];
		return target && action ? { target, action } : undefined;
	}

	/** Decisions made this session, for `/rayline`. Lookups are billed on encoded input. */
	readonly stats = { decisions: 0, encodedTokens: 0, latencyMs: 0, elapsedMs: 0, unrouted: 0, last: "none yet" };
	/** The last lookup: wall time here (network included) and the router's own `latency_ms`. */
	private lastTiming = { elapsedMs: 0, routerMs: undefined as number | undefined };

	private get showTiming() {
		return this.config.show_decision_time === true || this.pi.getFlag(TIMING_FLAG) === true;
	}

	/** Footer line for this turn, when timing is on. */
	private showDecision(ctx: ExtensionContext, text: string) {
		if (this.showTiming) ctx.ui.setStatus(PROVIDER, text);
	}

	private piModel(ctx: ExtensionContext, id: string) {
		const [provider, ...rest] = id.split("/");
		return ctx.modelRegistry.find(provider, rest.join("/"));
	}

	async route(request: Request, ctx: ExtensionContext): Promise<Route> {
		const view = toWire(request.messages);
		const head = view.wire.length > 0 ? sha256(JSON.stringify(view.wire[0])) : "";
		const rewritten = request.state !== undefined && request.state.head !== head;
		const sidecall = request.reason === "direct";
		let state: RouterState = !request.state || (rewritten && !sidecall) ? { head } : request.state;
		const keep = (served: { model: Route["model"]; thinkingLevel?: Level }): Route => ({
			model: served.model,
			thinkingLevel: served.thinkingLevel ?? "off",
			state,
		});

		// Pi's own requests (compaction summaries) are not worth a lookup.
		if (sidecall) return this.unrouted(request, ctx, state, undefined);
		let excluded: Target | undefined;
		if (request.reason === "retry" && request.failed && !rewritten) {
			// Pi retries the same prefix after a provider error. Stay on that model
			// unless the provider is overloaded; then ask again without it.
			const failure = (request.failed as { message?: { errorMessage?: string } }).message?.errorMessage ?? "";
			if (!OVERLOADED.test(failure)) {
				this.showDecision(ctx, "route: retry, no lookup");
				return keep(request.failed);
			}
			excluded = this.byPi.get(`${request.failed.model.provider}/${request.failed.model.id}`);
		}
		if (request.reason === "continuation" && request.previous && !rewritten && !this.config.redecide_on_continuation) {
			this.showDecision(ctx, "route: kept, no lookup");
			return keep(request.previous);
		}
		if (state.oversized) return this.unrouted(request, ctx, state, undefined);

		const started = performance.now();
		try {
			const body = this.decisionBody(request, ctx, { ...view, head, rewritten, sidecall }, excluded);
			const decision = await this.decide(body, ctx, request.signal);
			const route = this.resolve(decision, request, ctx);
			const { elapsedMs } = this.lastTiming;
			this.stats.last = `${decision.route_id ?? "route"} -> ${route.model.provider}/${route.model.id} in ${elapsedMs} ms`;
			this.showDecision(ctx, `route: ${elapsedMs} ms`);
			return { ...route, state };
		} catch (error) {
			if (request.signal?.aborted) throw error;
			if (error instanceof Refused && error.status === 413) state = { head, oversized: true };
			else if (error instanceof Refused && error.fatal) throw error;
			this.showDecision(ctx, `route: failed after ${Math.round(performance.now() - started)} ms`);
			if ((this.config.on_error ?? "previous") === "fail") throw error;
			return this.unrouted(request, ctx, state, error);
		}
	}

	/** The model for a request Decisions did not choose: the last one that answered, else the fallback. */
	private unrouted(request: Request, ctx: ExtensionContext, state: RouterState, error: unknown): Route {
		const fallback = this.config.fallback ? this.piModel(ctx, this.config.fallback) : undefined;
		// With no network a cloud model cannot answer either, so the last model is
		// the wrong choice if it was a cloud one: prefer a local target.
		const local = isOffline(error)
			? this.config.targets.filter((target) => target.placement === "local").map((target) => this.piModel(ctx, target.pi)).find(Boolean)
			: undefined;
		const model = local ?? request.previous?.model ?? fallback;
		if (!model) throw error ?? new Error("Rayline has not routed this session yet and no fallback model is configured");
		if (error !== undefined) {
			this.stats.unrouted += 1;
			this.stats.last = `unrouted -> ${model.provider}/${model.id}`;
			ctx.ui.notify(`Rayline routing unavailable, using ${model.id}: ${String(error)}`, "warning");
		}
		return { model, thinkingLevel: (local ? undefined : request.previous?.thinkingLevel) ?? "off", state };
	}

	private decisionBody(
		request: Request,
		ctx: ExtensionContext,
		view: ReturnType<typeof toWire> & { head: string; rewritten: boolean; sidecall: boolean },
		excluded: Target | undefined,
	) {
		const routing: Record<string, unknown> = { ...this.config.decisions.routing };
		const targets = this.config.targets.filter((target) => target !== excluded);
		if (this.v2) {
			routing.execution = this.executionContext(request, ctx, view, targets);
		} else {
			routing.models = targets.map((target) => target.model);
			const priced = targets.filter((target) => target.prices);
			if (priced.length > 0) routing.prices = Object.fromEntries(priced.map((target) => [target.model, target.prices]));
		}
		const system = view.system || ctx.getSystemPrompt();
		return {
			// The public Routes API decides for an ARC id; it refuses `rayline-router`.
			model: this.config.decisions.model ?? "arc-swift-1.0",
			max_tokens: this.config.max_tokens ?? 32_000,
			...(system ? { system } : {}),
			messages: view.wire,
			tools: activeTools(this.pi),
			routing,
		};
	}

	/** `routing.execution`, version arc-execution-context.v2, from what Pi states. */
	private executionContext(
		request: Request,
		ctx: ExtensionContext,
		view: ReturnType<typeof toWire> & { head: string; rewritten: boolean; sidecall: boolean },
		targets: Target[],
	) {
		const { wire, assistants } = view;
		const history = view.sidecall
			? []
			: [...assistants].flatMap(([message, assistant]) => {
					const bound = this.actionOf(
						{ provider: assistant.provider, id: assistant.model },
						(assistant as { thinkingLevel?: string }).thinkingLevel,
					);
					return bound ? [{ message, action_id: bound.action.action_id }] : [];
				});
		const served = view.sidecall ? undefined : this.actionOf(request.previous?.model, request.previous?.thinkingLevel);
		const last = wire.at(-1);
		const hasAssistant = wire.some((message) => message.role === "assistant");
		const event = (kind: string | null, compaction: unknown = null) => ({
			kind,
			basis: kind ? "explicit_client" : "unknown",
			compaction,
		});
		let stated = event(null);
		if (view.sidecall) {
			stated = event("sidecall");
		} else if (view.rewritten && String(wire[0]?.content[0]?.text ?? "").startsWith(COMPACTION_PREFIX)) {
			// Right after a compaction nothing new has been answered, so any
			// assistant message after the summary is Pi's retained tail.
			stated = event("compaction", { summary_message: 0, retained_from: hasAssistant ? 1 : null, new: true });
		} else if (request.reason === "user") {
			stated = event(hasAssistant ? "user_message" : "task_start");
		} else if (last?.content.some((block) => block.type === "tool_result")) {
			stated = event("tool_result");
		}
		const session = ctx.sessionManager.getSessionId();
		return {
			version: CONTEXT_VERSION,
			turn: {
				id: sha256(`${session}\0main\0${wire.length}\0${sha256(JSON.stringify(last ?? null))}`),
				attempt: 1,
				epoch: view.head,
				retry_of: null,
				reconsult: null,
			},
			event: stated,
			last_served: served
				? {
						action_id: served.action.action_id,
						worker: served.target.worker,
						message: history.findLast((entry) => entry.action_id === served.action.action_id)?.message ?? null,
					}
				: null,
			history,
			hold: null,
			expect: null,
			targets: targets.flatMap((target) =>
				(target.actions ?? []).map((action) => ({
					action_id: action.action_id,
					worker: target.worker,
					placement: target.placement,
					cache: null,
					capacity: target.placement === "local" ? this.localCapacity(ctx, target, view) : null,
				})),
			),
		};
	}

	/**
	 * Whether this prompt fits a local target's window, as the core states it
	 * for a model it has no tokenizer for: an upper bound from the prompt's
	 * UTF-8 bytes (no byte-level BPE token is shorter than one byte), plus 5%
	 * headroom, a template margin and the output allowance. prefer-local leaves
	 * out a local target whose capacity is unknown, so this must be sent.
	 * An image makes the bound a guess (`caller_estimated`), which the service
	 * does not admit as a fit.
	 */
	private localCapacity(ctx: ExtensionContext, target: Target, view: ReturnType<typeof toWire>) {
		const available = target.context_window ?? this.piModel(ctx, target.pi)?.contextWindow;
		if (!available) return null;
		const prompt = JSON.stringify({ system: view.system || ctx.getSystemPrompt(), messages: view.wire, tools: activeTools(this.pi) }, null, 4);
		const bytes = Buffer.byteLength(prompt, "utf8");
		const output = Math.min(this.config.max_tokens ?? 32_000, this.piModel(ctx, target.pi)?.maxTokens ?? Infinity);
		const required = Math.ceil(bytes * 1.05) + TEMPLATE_MARGIN_TOKENS + output;
		const fits = required <= available;
		const images = view.wire.some((message) => message.content.some((block) => block.type === "image"));
		return {
			fits,
			reason: fits ? null : "context_capacity",
			required,
			available,
			available_source: "configured",
			units: null,
			provenance: images ? "caller_estimated" : "caller_bounded",
		};
	}

	private async decide(body: unknown, ctx: ExtensionContext, signal?: AbortSignal) {
		const { decisions } = this.config;
		const key = await ctx.modelRegistry.getApiKeyForProvider(PROVIDER);
		if (!key) throw new Refused(401, "no Rayline key: run /login rayline");
		// The cell's budget is 1.5 s end to end; past that, fall back.
		const timeout = AbortSignal.timeout(decisions.timeout_ms ?? 3_000);
		const started = performance.now();
		const response = await fetch(decisions.url, {
			method: "POST",
			redirect: "error",
			signal: signal ? AbortSignal.any([signal, timeout]) : timeout,
			headers: {
				"content-type": "application/json",
				authorization: `Bearer ${key}`,
				"x-conversation-id": sha256(ctx.sessionManager.getSessionId()),
				"x-rayline-branch-id": sha256("main"),
				"x-rayline-wire-format": WIRE_FORMAT,
				...(decisions.cell_tag ? { "x-rayline-cell": decisions.cell_tag } : {}),
			},
			body: JSON.stringify(body),
		});
		if (!response.ok) {
			const code = await response
				.json()
				.then((refusal) => refusal?.error?.code)
				.catch(() => undefined);
			throw new Refused(response.status, code);
		}
		const result = await response.json();
		const elapsedMs = Math.round(performance.now() - started);
		const routerMs = typeof result.latency_ms === "number" ? Math.round(result.latency_ms) : undefined;
		this.lastTiming = { elapsedMs, routerMs };
		this.stats.decisions += 1;
		this.stats.encodedTokens += Number(result.usage?.encoded_input_tokens ?? 0);
		this.stats.latencyMs += routerMs ?? 0;
		this.stats.elapsedMs += elapsedMs;
		const which = decisions.result ?? "top";
		const selected = which === "top" ? result : which === "custom" ? result.custom : result.presets?.[which];
		if (!selected) throw new Error(`Decisions returned no ${which} result`);
		if (this.v2 && result.routing?.execution?.status !== "applied") {
			throw new Error("Decisions did not apply this v2 execution context");
		}
		// A preset's result carries no route id of its own; the lookup's applies.
		return { route_id: result.route_id, ...selected } as {
			route_id?: string;
			model?: string;
			worker?: string;
			thinking?: { mode?: string; budget_tokens?: number | null };
			thinking_level?: string;
			policy_action_id?: string;
			identity?: { action_id?: string };
		};
	}

	/** The Pi model and thinking level a decision names. Never substitutes another target. */
	private resolve(decision: Awaited<ReturnType<Router["decide"]>>, request: Request, ctx: ExtensionContext) {
		let target: Target | undefined;
		let thinkingLevel: Level = request.thinkingLevel;
		if (this.v2) {
			const bound = this.byAction.get(decision.identity?.action_id ?? decision.policy_action_id ?? "");
			target = bound?.target;
			if (bound) thinkingLevel = bound.action.thinking_level;
		} else {
			target = this.config.targets.find((entry) => entry.model === decision.model && entry.worker === decision.worker);
			target ??= this.config.targets.find((entry) => entry.model === decision.model);
			// `thinking_level` exists only in policy-service mode; `thinking` always does.
			// The live API reports "none" there when it restricts candidates; read as off.
			const level = (decision.thinking_level === "none" ? "off" : decision.thinking_level) as Level;
			thinkingLevel = LEVELS.includes(level) ? level : (levelOf(decision.thinking) ?? thinkingLevel);
		}
		if (!target) throw new Error(`Decisions selected ${decision.model ?? "an action"}, which is not a bound target`);
		const model = this.piModel(ctx, target.pi);
		if (!model) throw new Error(`Model ${target.pi} is not in Pi's catalog`);
		return { model, thinkingLevel };
	}
}

/**
 * Targets served through the Rayline gateway (`pi: "rayline/<gateway id>"`).
 * Rayline bills these turns, so Pi's own cost for them is zero.
 */
function gatewayModels(config: Config) {
	const prefix = `${PROVIDER}/`;
	return config.targets
		.filter((target) => target.pi.startsWith(prefix))
		.map((target) => ({
			id: target.pi.slice(prefix.length),
			name: `${target.model} via Rayline`,
			reasoning: true,
			input: ["text", "image"] as ("text" | "image")[],
			cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
			contextWindow: target.context_window ?? config.context_window ?? 200_000,
			maxTokens: config.max_tokens ?? 32_000,
		}));
}

export default function (pi: ExtensionAPI) {
	const config = loadConfig();
	pi.registerFlag(TIMING_FLAG, {
		description: "Show how long each Rayline route lookup took, in the footer",
		type: "boolean",
		default: false,
	});
	const router = new Router(config, pi);
	// The provider exists to hold the key: `/login rayline` stores it in auth.json.
	// The provider holds the Rayline key (`/login rayline` stores it in auth.json)
	// and serves any target whose Pi model is `rayline/<id>` through the Rayline
	// gateway, pinned to that model: one key for routing and cloud turns.
	pi.registerProvider(PROVIDER, {
		name: "Rayline",
		baseUrl: new URL(config.decisions.url).origin,
		api: "anthropic-messages",
		models: gatewayModels(config),
	});
	pi.registerVirtualModel<RouterState>({
		provider: PROVIDER,
		id: "arc",
		name: "Rayline ARC",
		// ARC selects the thinking level; the user's level is not an input.
		thinkingLevels: ["off"],
		contextWindow: config.context_window,
		maxTokens: config.max_tokens,
		route: (request, ctx) => router.route(request, ctx),
	});
	pi.registerCommand("rayline", {
		description: "Show Rayline routing for this session",
		handler: async (_args, ctx) => {
			const { decisions, encodedTokens, latencyMs, elapsedMs, unrouted, last } = router.stats;
			const mean = (total: number) => (decisions ? Math.round(total / decisions) : 0);
			ctx.ui.notify(
				`Rayline: ${decisions} lookups, ${encodedTokens} encoded input tokens billed, mean ${mean(elapsedMs)} ms per lookup (${mean(latencyMs)} ms at the router), ${unrouted} unrouted. Last: ${last}`,
				"info",
			);
		},
	});
}
