/**
 * cwoffee tools for pi — spike version.
 *
 * The point of this file: a pi tool can be a THIN SHIM into the Rust engine.
 * There is no tool logic and no guardrail here. The guardrail is in rust-shim.rs.
 * Production shape (PI-CORE-PLAN Phase 2) is the same 30 lines per tool.
 *
 * Load with:  pi --mode rpc -e <this file>
 */
import { Type } from "typebox";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import path from "node:path";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const here = path.dirname(fileURLToPath(import.meta.url));
const SHIM = process.env.CWOFFEE_SHIM ?? path.join(here, "cwshim");

const ReadParams = Type.Object({ path: Type.String({ description: "Absolute path to read" }) });

/** Call the Rust tool binary. stdin: JSON args, stdout: JSON envelope. */
function rust(tool: string, args: unknown): string {
	return execFileSync(SHIM, [tool], { input: JSON.stringify(args), encoding: "utf8" });
}

export default function (pi: ExtensionAPI) {
	pi.registerTool({
		name: "read_file",
		label: "cwoffee read_file",
		description: "Read a file through the cwoffee Rust engine. Guardrails are enforced by the engine.",
		promptSnippet: "read_file: read a file via the cwoffee rust engine",
		parameters: ReadParams,

		async execute(_toolCallId, params, _signal, _onUpdate, ctx) {
			// Approval round trip, cwoffee style: the RPC client (desktop / phone) answers.
			// `timeout` makes pi auto-resolve to a deny, so a vanished UI can never leave the
			// agent running things unattended — the same fail-closed contract as ApprovalRequested.
			const ok = await ctx.ui.confirm("Allow read_file?", params.path, { timeout: 15_000 });
			if (!ok) {
				return { content: [{ type: "text", text: "blocked: user denied approval" }], isError: true };
			}

			const envelope = rust("read_file", params); // <- the work happens in Rust
			return { content: [{ type: "text", text: envelope }] };
		},
	});
}
