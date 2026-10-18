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

function cwtool(sub: string, tool: string, args: unknown, permits: string[] = []): unknown {
	if (!CWTOOL) throw new Error("CWOFFEE_CWTOOL is not set — the cwoffee tool host cannot start");
	const raw = execFileSync(CWTOOL, [sub, tool, ...permits], {
		input: JSON.stringify(args ?? {}),
		encoding: "utf8",
		maxBuffer: 64 * 1024 * 1024,
	});
	try {
		return JSON.parse(raw);
	} catch {
		return { ok: false, error: `cwtool ${sub} ${tool} returned something that is not JSON` };
	}
}

function specs(): any[] {
	const res = cwtool("specs", "", {}) as any;
	if (!res?.ok) throw new Error(`cwtool specs failed: ${res?.error ?? "no specs"}`);
	return res.tools ?? [];
}

export default function (pi: ExtensionAPI) {
	let tools: any[] = [];
	try {
		tools = specs();
	} catch (e) {
		// Loud on stderr rather than silent: an extension that registers nothing and says
		// nothing is how a session ends up with a model that has no tools and no idea why.
		console.error(`[cwoffee-tools] no tools: ${(e as Error).message}`);
		return;
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

			async execute(_id, params, _signal, _onUpdate, ctx) {
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

				const res = cwtool("run", name, params, permits) as any;
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
