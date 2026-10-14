/**
 * drive-rpc.mjs — Phase 0 spike driver.
 *
 * Spawns `pi --mode rpc`, points it at cwoffee's OWN scripted endpoint
 * (scripts/mock-llm.mjs), drives one turn, answers approval prompts like a UI would,
 * and samples PSS from /proc/<pid>/smaps_rollup the same way scripts/memory.sh does.
 *
 * Run:  node spike/drive-rpc.mjs
 * Needs: mock-llm listening (default 127.0.0.1:8190), spike/cwshim built (build-shim.sh).
 */
import { spawn } from "node:child_process";
import { mkdirSync, readFileSync, writeFileSync, symlinkSync, existsSync, realpathSync, copyFileSync } from "node:fs";
import { execSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const PI_BIN = process.env.PI_BIN ?? "pi";
const AGENT_DIR = process.env.PI_CODING_AGENT_DIR ?? "/tmp/pi-probe";
const WORKDIR = process.env.PI_WORKDIR ?? "/tmp/pi-ws";
const PROVIDER = process.env.PROVIDER ?? "cwoffee-mock";
const MODEL = process.env.MODEL ?? "cwoffee-mock";
const EXT_SRC = path.join(here, "pi-extension-cwoffee-tools.ts");
const SHIM = process.env.CWOFFEE_SHIM ?? path.join(here, "cwshim");
const SPAWN_EXTENSION = process.env.SPAWN_EXTENSION !== "0";
const PROMPT =
  process.env.PROMPT ?? "Use read_file to read the file /etc/hostname"; // scenarios.json: read-file-approval

/** PSS in MB, read from smaps_rollup (PSS, never RSS — PLAN.md §memory). */
function pss(pid) {
  try {
    for (const l of readFileSync(`/proc/${pid}/smaps_rollup`, "utf8").split("\n")) {
      if (l.startsWith("Pss:")) return Math.round(parseInt(l.trim().split(/\s+/)[1], 10) / 102.4) / 10;
    }
  } catch {}
  return null;
}

/** Find the node_modules of the installed pi so the extension's imports resolve. */
function piNodeModules() {
  const bin = realpathSync(execSync(`command -v ${PI_BIN}`, { shell: "bash" }).toString().trim());
  let d = path.dirname(bin);
  for (let i = 0; i < 6 && d !== "/"; i++, d = path.dirname(d)) {
    const cand = path.join(d, "node_modules", "@earendil-works", "pi-coding-agent");
    if (existsSync(cand)) return path.join(d, "node_modules");
  }
  throw new Error("could not find @earendil-works/pi-coding-agent next to the installed pi");
}

// --- isolated pi config: nothing here writes to ~/.pi, and PI_OFFLINE stops network chatter
mkdirSync(AGENT_DIR, { recursive: true });
mkdirSync(WORKDIR, { recursive: true });
writeFileSync(path.join(AGENT_DIR, "models.json"), readFileSync(path.join(here, "models.json"), "utf8"));
const extDst = path.join(AGENT_DIR, "extensions", "cwoffee-tools.ts");
mkdirSync(path.dirname(extDst), { recursive: true });
copyFileSync(EXT_SRC, extDst);
if (!existsSync(path.join(AGENT_DIR, "node_modules"))) {
  symlinkSync(piNodeModules(), path.join(AGENT_DIR, "node_modules"));
}

const args = ["--mode", "rpc", "--no-session", "--provider", PROVIDER, "--model", MODEL];
if (SPAWN_EXTENSION) args.push("-e", extDst);

const p = spawn(PI_BIN, args, {
  cwd: WORKDIR,
  env: {
    ...process.env,
    PI_CODING_AGENT_DIR: AGENT_DIR,
    PI_OFFLINE: "1",
    PI_SKIP_VERSION_CHECK: "1",
    CWOFFEE_SHIM: SHIM,
  },
  stdio: ["pipe", "pipe", "pipe"],
});

const samples = [];
const timer = setInterval(() => samples.push(pss(p.pid)), 400);
const usage = [];
const seenUsage = new Set();
let ready = false;
let buf = "";
const t0 = Date.now();

// Protocol framing: bytes, split on LF only. Node's readline would also split on
// U+2028/U+2029, which are legal inside JSON strings (docs/rpc.md).
p.stdout.on("data", (d) => {
  buf += d.toString();
  let i;
  while ((i = buf.indexOf("\n")) >= 0) {
    const line = buf.slice(0, i).trim();
    buf = buf.slice(i + 1);
    if (!line) continue;
    let r;
    try {
      r = JSON.parse(line);
    } catch {
      continue;
    }

    if (r.type === "extension_ui_request") {
      console.log(`← APPROVAL  ${r.method} ${JSON.stringify({ title: r.title, message: r.message, timeout: r.timeout })}`);
      setTimeout(() => p.stdin.write(JSON.stringify({ type: "extension_ui_response", id: r.id, confirmed: true }) + "\n"), 400);
      continue;
    }
    const u = r.usage ?? (r.type === "message_end" ? r.message?.usage : undefined);
    if (u && (u.input || u.cacheRead || u.output)) {
      const row = { at: r.type, in: u.input, cacheRead: u.cacheRead, cacheWrite: u.cacheWrite, out: u.output };
      const key = JSON.stringify(row);
      if (!seenUsage.has(key)) { seenUsage.add(key); usage.push(row); }
    }
    if (r.type === "response") {
      if (r.command === "get_state" && !ready) {
        ready = true;
        console.log(`← READY      ${Date.now() - t0}ms after spawn (model ${r.data?.model?.id}, ctx ${r.data?.model?.contextWindow})`);
        setTimeout(() => {
          console.log(`→ PROMPT    ${JSON.stringify(PROMPT)}\n`);
          p.stdin.write(JSON.stringify({ id: "1", type: "prompt", message: PROMPT }) + "\n");
        }, 300);
      }
      console.log(`← RESPONSE  ${r.command} success=${r.success} ${JSON.stringify(r.data ?? {})}`.slice(0, 160));
    } else if (r.type === "tool_execution_start") console.log(`← TOOL START ${r.toolName} ${JSON.stringify(r.args)}`);
    else if (r.type === "tool_execution_end")
      console.log(`← TOOL END   ${r.toolName} isError=${r.isError} dur=${r.durationMs}ms\n             ${JSON.stringify(r.result?.content).slice(0, 200)}`);
    else if (r.type === "message_update") {
      const e = r.assistantMessageEvent;
      if (e && e.type !== "text_delta" && e.type !== "thinking_delta") console.log(`← STREAM    ${e.type}`);
    } else if (["agent_start", "turn_start", "turn_end", "agent_end", "agent_settled", "compaction_start", "compaction_end", "extension_error", "auto_retry_start"].includes(r.type))
      console.log(`← EVENT     ${r.type} ${JSON.stringify(r).slice(0, 120)}`);
    if (r.type === "response" && !r.success) console.log(`!! FAILED   ${r.command}: ${r.error}`);
  }
});
p.stderr.on("data", (d) => {
  const s = d.toString();
  if (/error|extension/i.test(s)) process.stderr.write(`[pi stderr] ${s}`);
});

// state request immediately: its response is the readiness signal, and its latency is
// the cold start the desktop app pays per workspace.
p.stdin.write(JSON.stringify({ id: "boot", type: "get_state" }) + "\n");

const DONE = process.env.PROMPT ? 20_000 : 14_000;
setTimeout(async () => {
  clearInterval(timer);
  const live = samples.filter(Boolean);
  console.log("\n--- SUMMARY ---");
  console.log(`usage: ${JSON.stringify(usage, null, 1)}`);
  console.log(`PSS:   idle ${live[0]} MB | peak ${Math.max(...live)} MB | final ${pss(p.pid)} MB`);
  console.log(`note:  total prompt tokens = usage.input + cacheRead + cacheWrite (input EXCLUDES cached)`);
  p.stdin.end();
  await new Promise((r) => setTimeout(r, 1200));
  p.kill("SIGKILL");
  process.exit(0);
}, DONE);
