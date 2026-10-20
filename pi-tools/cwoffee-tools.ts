/**
 * cwoffee's real tools, mounted on pi — production shape.
 *
 * There is no tool logic in this file, and that is the design. Everything about what a
 * tool *is* (name, description, schema) and everything about what a call *may do* (risk,
 * escapes, the one-line summary a human reads on the approval card) comes out of the Rust
 * engine via `cwtool`. So there is one definition of each tool, not one here and one in
 * the engine with a guaranteed drift between them — a tool description is prompt bytes,
 * and two copies means two prefix caches and one lie waiting to happen.
 *
 * Load with:  pi --mode rpc -e <this file>      (CWOFFEE_CWTOOL must point at cwtool)
 *
 * Compare with spike/pi-extension-cwoffee-tools.ts: that file proved the seam with one
 * hand-written tool and a fake guardrail. This one is what the seam is for.
 */
import { execFileSync } from "node:child_process";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const CWTOOL = process.env.CWOFFEE_CWTOOL ?? "";
// Fail-closed, like cwoffee's ApprovalRequested: if the UI is gone, pi resolves to a deny.
const APPROVAL_TIMEOUT_MS = Number(process.env.CWOFFEE_APPROVAL_TIMEOUT_MS ?? 15_000);
// Longer than an approval on purpose, and it matches the engine's own default: an
// unanswered approval costs nothing, while a question is a decision the user may be
// thinking about. The bridge mirrors the timeout from the card side anyway.
const QUESTION_TIMEOUT_MS = Number(process.env.CWOFFEE_QUESTION_TIMEOUT_MS ?? 180_000);
// Off by default: this extension runs inside the model's process, and chatter on stderr
// lands in whoever is reading the bridge's log. One flag turns it on when the question
// round trip is the thing being debugged.
const DEBUG = /^(1|true|yes)$/i.test(process.env.CWOFFEE_TOOLS_DEBUG ?? "");

/**
 * The marker cwoffee's bridge looks for at the start of a dialog title to recover the
 * engine's own question — each option's one-line trade-off, which option is recommended,
 * whether several may be picked, and which set this question belongs to. pi's dialog
 * primitives carry a title and a list of labels, so without this the card the user
 * answers would be a flattened summary of the one the engine designed. Keep in step with
 * CANON_Q in crates/bridge/src/host.rs.
 */
const CANON_Q = "CWOFFEEQ ";

/**
 * The bridge answers a pi `select` with one label; a cwoffee card can pick several. Picks
 * therefore travel joined with this separator (see the reply in the bridge) and are split
 * back into option indexes here. Option labels are "few words" by the engine's own schema,
 * so they do not contain it; if one somehow does, nothing matches and the answer is
 * reported as free text instead of as an invented pick.
 */
function splitPicks(value: string, labels: string[]): number[] {
	const picks: number[] = [];
	for (const part of value.split(" | ").map((p) => p.trim()).filter((p) => p.length > 0)) {
		const i = labels.indexOf(part);
		if (i >= 0) picks.push(i);
	}
	return picks;
}

/**
 * One question, asked in the host's UI, answered as the engine's `Answer` shape.
 *
 * Every question of a set is in flight at once, because a cwoffee card holds a set as one
 * card with Back/Next and the engine budgets one timeout for the whole set rather than one
 * per question — asking them one after another would make the user answer three cards and
 * would be punished by a budget the tool already sized for a single reading.
 */
async function askOne(ctx: any, q: any, callId: string): Promise<unknown> {
	// `k` names this call for the bridge, and it has to name *this* call: `cwtool` numbers a
	// fresh set in every process, so the engine's own set id is 1 for the first set of every
	// call in every conversation. Key on pi's tool-call id, which is unique per call, and two
	// ask_user calls in one conversation stay two cards instead of merging into one.
	const key = callId;
	const title = `${CANON_Q}${JSON.stringify({ k: key, q })}`;
	const labels: string[] = (q.options ?? []).map((o: any) => String(o.label));
	if (labels.length > 0) {
		const picked = await ctx.ui.select(title, labels, { timeout: QUESTION_TIMEOUT_MS });
		if (DEBUG) console.error(`[cwoffee-tools] select ${JSON.stringify(q.question)} -> ${JSON.stringify(picked)}`);
		if (picked === undefined || picked === null) {
			console.error(`[cwoffee-tools] nobody answered "${String(q.question).slice(0, 60)}" — the tool will say so rather than guess`);
			return null;
		}
		const value = typeof picked === "string" ? picked : String(picked);
		const picks = splitPicks(value, labels);
		return picks.length > 0 ? { text: value, options: picks } : { text: value, options: [] };
	}
	const typed = await ctx.ui.input(title, "type an answer", { timeout: QUESTION_TIMEOUT_MS });
	if (DEBUG) console.error(`[cwoffee-tools] input ${JSON.stringify(q.question)} -> ${JSON.stringify(typed)}`);
	if (typed === undefined || typed === null) {
		console.error(`[cwoffee-tools] nobody answered "${String(q.question).slice(0, 60)}" — the tool will say so rather than guess`);
		return null;
	}
	return { text: String(typed), options: [] };
}

// Which conversation this pi session belongs to, in cwoffee's own terms. The bridge keeps
// the map from row to pi session file; the tool host reads it. Passing pi's own session file
// rather than a row number means the answer comes from a fact pi knows instead of a label the
// extension guessed — and when the map has no entry yet, `recall` says which claim it cannot
// make instead of inventing one.
function piSessionFile(ctx: any): string {
	try {
		return ctx?.sessionManager?.getSessionFile?.() ?? "";
	} catch {
		return "";
	}
}

function cwtool(sub: string, tool: string, args: unknown, permits: string[] = [], extra: string[] = [], sessionFile = ""): unknown {
	if (!CWTOOL) throw new Error("CWOFFEE_CWTOOL is not set — the cwoffee tool host cannot start");
	const raw = execFileSync(CWTOOL, [sub, tool, ...permits, ...extra], {
		input: JSON.stringify(args ?? {}),
		encoding: "utf8",
		maxBuffer: 64 * 1024 * 1024,
		env: { ...process.env, CWOFFEE_PI_SESSION_FILE: sessionFile },
	});
	try {
		return JSON.parse(raw);
	} catch {
		return { ok: false, error: `cwtool ${sub} ${tool} returned something that is not JSON` };
	}
}

function specs(): any {
	const res = cwtool("specs", "", {}) as any;
	if (!res?.ok) throw new Error(`cwtool specs failed: ${res?.error ?? "no specs"}`);
	return res;
}

/**
 * How much of the tool set the model is *shown*.
 *
 * The bridge says `lean` or `full`, and the registry (through `cwtool specs`) says which
 * tools are always declared. Neither is decided here: this file is plumbing, and a list of
 * "the important tools" written in TypeScript would be a second policy answering the same
 * question differently from the engine.
 *
 * Narrowing matters because tool declarations are prompt bytes measured in the request body
 * (~12 KB for the whole set), and on a small local model with a small window that is a fifth
 * of the room spent describing capabilities the turn will not use. `explore` is how the rest
 * comes back: the model describes the job, the registry searches itself, and the tools it
 * names are made callable here.
 */
const TOOL_MODE = (process.env.CWOFFEE_TOOL_MODE ?? "full").trim().toLowerCase();

/** The set this process declared last, kept so activation is a union and not a guess. */
let declared: string[] = [];

/** Declare exactly these tools. Returns a sentence when it could not, for the caller to put
 *  in front of the model rather than in a log nobody reads. */
function declareExactly(pi: any, names: string[]): string | null {
	if (typeof pi.setActiveTools !== "function") {
		return "this pi build cannot change the declared tool set, so the full set stays declared";
	}
	const missing = names.filter((n) => !registered.has(n));
	declared = [...names];
	pi.setActiveTools(declared);
	if (missing.length) return `the registry named tools that are not mounted here: ${missing.join(", ")}`;
	return null;
}

/** Add tools to what is already declared. Used by `explore`: a discovered tool that the host
 *  does not actually make callable is a promise the model was lied to about. */
function declareMore(pi: any, names: string[]): string | null {
	if (typeof pi.setActiveTools !== "function") {
		return "this pi build cannot change the declared tool set, so the tools `explore` found are not callable";
	}
	const have = new Set<string>(declared.length ? declared : registered);
	for (const n of names) if (registered.has(n)) have.add(n);
	declared = [...have];
	pi.setActiveTools(declared);
	const skipped = names.filter((n) => !registered.has(n));
	if (skipped.length) return `not mounted in this process, so not made callable: ${skipped.join(", ")}`;
	return null;
}

const registered = new Set<string>();

export default function (pi: ExtensionAPI) {
	let spec: any = {};
	let tools: any[] = [];
	try {
		spec = specs();
		tools = spec.tools ?? [];
	} catch (e) {
		// Loud on stderr rather than silent: an extension that registers nothing and says
		// nothing is how a session ends up with a model that has no tools and no idea why.
		console.error(`[cwoffee-tools] no tools: ${(e as Error).message}`);
		return;
	}

	for (const spec of tools) registered.add(spec.name as string);

	if (TOOL_MODE === "lean") {
		const lean: string[] = Array.isArray(spec.lean) ? spec.lean : [];
		if (!lean.length) {
			console.error("[cwoffee-tools] lean mode asked for but the registry gave no always-declared set; the full set stays declared");
		} else {
			const problem = declareExactly(pi, lean);
			if (problem) console.error(`[cwoffee-tools] lean mode did not apply: ${problem}`);
			else console.error(`[cwoffee-tools] lean mode: declaring ${lean.join(", ")} of ${registered.size} tools; the rest are behind \`explore\``);
		}
	}

	for (const spec of tools) {
		const name = spec.name as string;
		pi.registerTool({
			name,
			label: `cwoffee ${name}`,
			// The engine's own description, verbatim. Editing it here would fork the prompt.
			description: spec.description,
			promptSnippet: `${name}: ${String(spec.description).split("\n")[0].slice(0, 90)}`,
			// JSON Schema from the engine's `schema()`. pi's parameter schema *is* JSON
			// Schema, so the registry's definition is used without being rewritten.
			parameters: spec.parameters,

			async execute(callId, params, _signal, _onUpdate, ctx) {
				// Ask the engine how risky this specific call is. Not a guess from the tool
				// name, and not pi's dialog text: reading `../.ssh/id_ed25519` and reading
				// `notes.md` are different decisions and the card has to say which.
				const chk = cwtool("check", name, params) as any;
				if (!chk?.ok) {
					return { content: [{ type: "text", text: `blocked: ${chk?.error ?? "the tool could not be assessed"}` }], details: chk, isError: true };
				}
				if (chk.needs_approval) {
					const yes = await ctx.ui.confirm(`Allow ${name}?`, chk.summary, { timeout: APPROVAL_TIMEOUT_MS });
					if (!yes) {
						return { content: [{ type: "text", text: "blocked: user denied approval" }], details: chk, isError: true };
					}
				}
				// The escapes the user just saw are the permits for THIS call only. The engine
				// rebuilds its permits per call for the same reason: an approval with a memory
				// is a hole.
				const permits: string[] = [];
				for (const p of chk.escapes ?? []) permits.push("--permit", p);

				if (spec.host_ask) {
					// The one tool whose result is a person. It runs in a short-lived `cwtool`
					// process, which cannot hold a question open on a live channel, so the
					// questions are read out of the engine (its own parse, its own option sets),
					// asked in the host's UI, and handed back with --host-answers. The answers are
					// a flag rather than part of `args` on purpose: `args` is model-authored, and a
					// model that can answer its own question is not asking one.
					const qs = cwtool("questions", name, params) as any;
					if (!qs?.ok) {
						return { content: [{ type: "text", text: `blocked: ${qs?.error ?? "the questions could not be built"}` }], details: qs, isError: true };
					}
					const asked = (qs.questions ?? []) as any[];
					const answers = await Promise.all(asked.map((q) => askOne(ctx, q, String(callId))));
					const res = cwtool(
						"run",
						name,
						params,
						permits,
						["--host-answers", JSON.stringify(answers)],
						piSessionFile(ctx),
					) as any;
					const askedText = typeof res?.rendered === "string" && res.rendered.length > 0 ? res.rendered : JSON.stringify(res);
					return { content: [{ type: "text", text: askedText }], details: res, isError: res?.ok !== true };
				}

				const res = cwtool("run", name, params, permits, [], piSessionFile(ctx)) as any;
				// A discovery result is a capability claim about the rest of this session, so
				// the host that can make it true has to say whether it did. The names come from
				// the registry (`activates` on the tool result); this file only carries them to
				// pi and reports back what happened.
				const found: string[] = Array.isArray(res?.activates)
					? res.activates.filter((x: unknown) => typeof x === "string")
					: [];
				if (found.length) {
					const problem = declareMore(pi, found);
					const note = problem
						? `\n\n[host] ${problem}`
						: `\n\n[host] made callable from your next call: ${found.join(", ")}`;
				if (res) {
					// The engine's own words first; this line is the host signing for the part
					// of the promise it owns. The envelope carries `rendered` (the sentence the
					// model reads) and `output` (the same text before rendering), and a host note
					// has to ride on the part that actually reaches the prompt - which is not the
					// field named `content`, a shape this envelope does not have.
					const base =
						typeof res.rendered === "string"
							? res.rendered
							: typeof res.output === "string"
								? res.output
								: "";
					res.rendered = `${base}${note}`;
				}
				}
				// What the model reads is the tool's own words (`render_result` in the
				// engine), not the envelope: a spreadsheet grid handed over as escaped JSON
				// costs more prompt tokens and reads worse. The envelope still travels, in
				// pi's `details`, which is kept out of the prompt and given to the UI and the
				// log — that is where the bridge reads `files` and the `+N -M` from.
				const text = typeof res?.rendered === "string" && res.rendered.length > 0 ? res.rendered : JSON.stringify(res);
				return { content: [{ type: "text", text }], details: res, isError: res?.ok !== true };
			},
		});
	}
	console.error(`[cwoffee-tools] ${tools.length} tool(s) mounted from the rust registry`);
}
