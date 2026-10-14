/**
 * pi-event-capture.mjs — record a real `pi --mode rpc` event stream, verbatim.
 *
 * The bridge translates pi's records into cwoffee's EngineEvent vocabulary. That
 * translation must be written against what pi actually sends, not against a doc's
 * example, and it must stay honest when pi is bumped. So this script records one
 * turn — streaming text, a tool call, an approval round trip, and the usage
 * trailer — into a fixture the Rust translator tests replay.
 *
 * Run:
 *   cd ../cwoffee && node scripts/mock-llm.mjs --port 8190            # scripted endpoint
 *   ./build-shim.sh && node ../scripts/pi-event-capture.mjs           # from spike/
 *
 * Output: crates/bridge/tests/fixtures/pi-events-<scenario>.jsonl, one record per
 * line exactly as pi framed it (no reformatting: a fixture that is prettier than
 * the wire stops catching framing bugs).
 */
import { spawn, execSync } from "node:child_process";
import { mkdirSync, readFileSync, writeFileSync, symlinkSync, existsSync, realpathSync, copyFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, "..");
const PI_BIN = process.env.PI_BIN ?? "pi";
const AGENT_DIR = process.env.PI_CODING_AGENT_DIR ?? "/tmp/pi-capture";
const WORKDIR = process.env.PI_WORKDIR ?? "/tmp/pi-ws";
const PROVIDER = process.env.PROVIDER ?? "cwoffee-mock";
const MODEL = process.env.MODEL ?? "cwoffee-mock";
const SCENARIO = process.env.SCENARIO ?? "read-file-approval";
const PROMPTS = {
  "read-file-approval": "Use read_file to read the file /etc/hostname",
  "count-fifteen": "Count from 1 to 15.",
};
const PROMPT = PROMPTS[SCENARIO] ?? PROMPTS["count-fifteen"];
const OUT = process.env.OUT ?? path.join(root, "crates", "bridge", "tests", "fixtures", `pi-events-${SCENARIO}.jsonl`);

function piNodeModules() {
  const bin = realpathSync(execSync(`command -v ${PI_BIN}`, { shell: "bash" }).toString().trim());
  let d = path.dirname(bin);
  for (let i = 0; i < 6 && d !== "/"; i++, d = path.dirname(d)) {
    if (existsSync(path.join(d, "node_modules", "@earendil-works", "pi-coding-agent"))) {
      return path.join(d, "node_modules");
    }
  }
  throw new Error("could not find @earendil-works/pi-coding-agent next to the installed pi");
}

mkdirSync(AGENT_DIR, { recursive: true });
mkdirSync(WORKDIR, { recursive: true });
writeFileSync(path.join(AGENT_DIR, "models.json"), readFileSync(path.join(here, "..", "spike", "models.json"), "utf8"));
const extDst = path.join(AGENT_DIR, "extensions", "cwoffee-tools.ts");
mkdirSync(path.dirname(extDst), { recursive: true });
copyFileSync(path.join(here, "..", "spike", "pi-extension-cwoffee-tools.ts"), extDst);
if (!existsSync(path.join(AGENT_DIR, "node_modules"))) {
  symlinkSync(piNodeModules(), path.join(AGENT_DIR, "node_modules"));
}

const args = ["--mode", "rpc", "--provider", PROVIDER, "--model", MODEL, "-e", extDst];
if (SCENARIO === "count-fifteen") args.push("--no-session");

const p = spawn(PI_BIN, args, {
  cwd: WORKDIR,
  env: {
    ...process.env,
    PI_CODING_AGENT_DIR: AGENT_DIR,
    PI_OFFLINE: "1",
    PI_SKIP_VERSION_CHECK: "1",
    CWOFFEE_SHIM: process.env.CWOFFEE_SHIM ?? path.join(here, "..", "spike", "cwshim"),
  },
  stdio: ["pipe", "pipe", "pipe"],
});

mkdirSync(path.dirname(OUT), { recursive: true });
const records = [];
let buf = "";
let settled = false;

// Split on LF only: pi frames JSONL with LF, and U+2028/U+2029 are legal inside a
// JSON string, so any unicode-aware line reader can cut a record in half.
p.stdout.on("data", (d) => {
  buf += d.toString();
  let i;
  while ((i = buf.indexOf("\n")) >= 0) {
    const line = buf.slice(0, i);
    buf = buf.slice(i + 1);
    if (!line.trim()) continue;
    let r;
    try {
      r = JSON.parse(line);
    } catch {
      console.log(`!! non-JSON line (${line.length} bytes) — the framer would have split here too`);
      continue;
    }
    records.push(line);
    if (r.type === "extension_ui_request") {
      setTimeout(
        () => p.stdin.write(JSON.stringify({ type: "extension_ui_response", id: r.id, confirmed: true }) + "\n"),
        300,
      );
    }
    if (r.type === "response" && r.command === "get_state") {
      setTimeout(() => p.stdin.write(JSON.stringify({ id: "1", type: "prompt", message: PROMPT }) + "\n"), 200);
    }
    if (r.type === "agent_settled") settled = true;
  }
});
p.stderr.on("data", (d) => {
  const s = d.toString();
  if (/error/i.test(s)) process.stderr.write(`[pi stderr] ${s}`);
});

p.stdin.write(JSON.stringify({ id: "boot", type: "get_state" }) + "\n");

const finish = async () => {
  writeFileSync(OUT, records.join("\n") + "\n");
  const kinds = {};
  for (const l of records) {
    const t = JSON.parse(l).type ?? "?";
    kinds[t] = (kinds[t] ?? 0) + 1;
  }
  console.log(`wrote ${records.length} records -> ${OUT}`);
  console.log(`record types: ${JSON.stringify(kinds, null, 1)}`);
  p.stdin.end();
  await new Promise((r) => setTimeout(r, 800));
  p.kill("SIGKILL");
  process.exit(0);
};
for (let t = 2000; t <= 20000; t += 1000) {
  setTimeout(() => {
    if (settled) finish();
  }, t);
}
setTimeout(finish, 25000);
