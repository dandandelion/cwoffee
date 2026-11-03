#!/usr/bin/env node
// One client, two hosts. `invoke('cmd', {...})` in the Svelte app goes either to the Tauri
// command of the same name or to the gateway's dispatch for the same verb, depending on where
// the window is running, and `transport.ts` passes the payload through unchanged. So an
// argument name is a promise made in three places at once, and the two hosts spell command
// parameters differently by default: Tauri wants camelCase unless the command says otherwise,
// and the gateway reads the engine's own snake_case off the frame.
//
// This has bitten for real. `respond_approval` once compared `"allow_session"` while the web
// UI sent `allowSession`, so a phone could press Allow and the turn stayed parked with an
// error naming an answer nobody had seen. And every multi-word argument on the endpoint
// screen — `base_url`, `api_key`, `context_window`, `max_tokens`, ... — was spelled for the
// gateway, which made Test connection, Discover models and Save answer
// "invalid args `baseUrl` ... missing required key baseUrl" on the desktop: settings verbs
// that had Rust tests speaking the server's own dialect, and no page case driving the button.
//
// So: read what the client sends, read what each host *requires*, and fail when a required
// name is one the client never sends. Extra keys a host ignores are not reported — that is
// normal and harmless. A verb no host implements is reported: the page can call into the void
// and, if the call site has a `.catch`, never find out.
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { dirname, join } from 'node:path'

const root = join(dirname(fileURLToPath(import.meta.url)), '..')
const read = (p) => readFileSync(join(root, p), 'utf8')
const camel = (s) => s.replace(/_([a-z0-9])/g, (_, c) => c.toUpperCase())
const stripRs = (s) => s.replace(/^\s*\/\/.*$/gm, '')
const stripTs = (s) => s.replace(/\/\/[^\n]*/g, '')

const UI = 'ui/src/App.svelte'
const GW = 'crates/gateway/src/lib.rs'
const TAURI = 'crates/tauri-app/src/commands.rs'
const ENGINE_FILES = [
  'crates/engine/src/config.rs',
  'crates/engine/src/host.rs',
  'crates/engine/src/settings.rs',
  'crates/engine/src/store.rs',
]

/* ---------------------------------- the client ---------------------------------- */

/** Top-level keys of every literal payload in an `invoke('cmd', { ... })` call. */
function clientCalls(src) {
  const calls = new Map() // cmd -> Set of keys
  const skipped = []
  const clean = stripTs(src)
  // The verb name is what matters, and a type argument must not be allowed to hide a call site.
  // The old pattern stopped a generic at the first `;`, which is exactly where a payload-typed
  // generic like `invoke<{ mime: string; data_b64: string }[]>('message_attachments', {...})` puts
  // one — so that call was invisible to this check, and the desktop command behind it was spelled
  // in the wrong dialect for two days. Instead: walk forward from `invoke` through an optional
  // generic, balancing nothing but "the first `(` at angle-depth 0 ends it", then read the verb.
  let m
  let sites = 0
  let unread = 0
  const ire = /\binvoke\s*/g
  while ((m = ire.exec(clean))) {
    let p = m.index + m[0].length
    sites++
    if (clean[p] === '<') {
      let angle = 0
      while (p < clean.length) {
        const ch = clean[p]
        if (ch === '<') angle++
        else if (ch === '>' && angle > 0) angle--
        else if (ch === '(' && angle === 0) break
        p++
      }
    }
    const rest = clean.slice(p, p + 90)
    const vm = rest.match(/^\(\s*'([a-z_][a-z0-9_]*)'\s*,?/)
    if (!vm) {
      // A call site this check cannot read is a call site it cannot protect. Say so out loud in the
      // tally rather than letting a clean verdict rest on a silent gap.
      unread++
      continue
    }
    const cmd = vm[1]
    let i = clean.indexOf('{', p + vm[0].length)
    const after = clean.slice(p + vm[0].length).trimStart()
    if (after.startsWith(')')) {
      // A call with no payload is still a call: both hosts have to answer it. Grading only verbs
      // that carry arguments left the zero-argument ones — `about`, `session_state`, `cancel` — free
      // to exist on one host and not the other, which is the exact drift this script is for: the
      // desktop answers and the phone's request dies with "unknown verb". Register the verb with no
      // keys, so existence is checked even though there is nothing to spell.
      if (!calls.has(cmd)) calls.set(cmd, new Set())
      continue
    }
    if (!after.startsWith('{') || i < 0 || i > p + 400) {
      if (after) skipped.push(`${cmd}: payload is not a literal object`)
      continue
    }
    let depth = 0
    let end = -1
    for (let j = i; j < clean.length; j++) {
      if (clean[j] === '{') depth++
      else if (clean[j] === '}') {
        depth--
        if (depth === 0) {
          end = j
          break
        }
      }
    }
    const body = clean.slice(i + 1, end)
    const named = []
    let d = 0
    let part = ''
    const parts = []
    for (const ch of body) {
      if ('{(['.includes(ch)) d++
      else if (')}]'.includes(ch)) d--
      if (ch === ',' && d === 0) {
        parts.push(part)
        part = ''
      } else part += ch
    }
    parts.push(part)
    for (const p of parts) {
      const t = p.trim()
      if (!t || t.startsWith('...')) continue
      const kv = t.match(/^['"]?([A-Za-z_][A-Za-z0-9_]*)['"]?\s*:/)
      if (kv) named.push(kv[1])
      else if (/^[A-Za-z_][A-Za-z0-9_]*$/.test(t)) named.push(t) // shorthand: the key is the variable
    }
    if (!named.length) {
      skipped.push(`${cmd}: payload has no readable keys`)
      continue
    }
    if (!calls.has(cmd)) calls.set(cmd, new Set())
    for (const k of named) calls.get(cmd).add(k)
  }
  return { calls, skipped, sites, unread }
}

/* ---------------------------------- the engine ---------------------------------- */

/** Struct fields by name: what a `from_value` dispatch accepts, and which of them the client
 * has to send. `#[serde(default)]` means the host can answer without it. */
function structFields(files) {
  const out = new Map()
  for (const f of files) {
    const src = stripRs(read(f))
    const re = /((?:#\[[^\]]*\]\s*)*)pub struct ([A-Za-z_][A-Za-z0-9_]*)[^{]*\{([\s\S]*?)\n\}/g
    let m
    while ((m = re.exec(src))) {
      const [, attrs, name, body] = m
      if (/rename_all/.test(attrs)) {
        out.set(name, { required: null, optional: new Set(), renamed: true })
        continue
      }
      const required = new Set()
      const optional = new Set()
      const aliases = new Map() // accepted alias -> canonical field
      let pendingDefault = false
      let pendingAliases = []
      for (const raw of body.split('\n')) {
        const l = raw.trim()
        if (l.startsWith('#[')) {
          if (/serde\s*\(/.test(l)) {
            if (l.includes('default')) pendingDefault = true
            for (const al of l.matchAll(/alias\s*=\s*"([^"]+)"/g)) pendingAliases.push(al[1])
          }
          continue
        }
        const f = l.match(/^pub\s+([a-z_][a-z0-9_]*)\s*:/)
        if (!f) continue
        const field = f[1]
        if (pendingDefault) optional.add(field)
        else required.add(field)
        for (const a of pendingAliases) aliases.set(a, field)
        pendingDefault = false
        pendingAliases = []
      }
      out.set(name, { required, optional, aliases, renamed: false })
    }
  }
  return out
}

/* ---------------------------------- the gateway --------------------------------- */

/** What each `"cmd" =>` arm reads: names it demands, names it takes or leaves, and aliases it
 * accepts for a key that changed spelling while some tab still holds the old bundle. */
function gatewayArms(src, structs) {
  const out = new Map()
  const clean = stripRs(src)
  const marks = [...clean.matchAll(/^ {8}"([a-z_][a-z0-9_]*)" =>/gm)]
  for (let k = 0; k < marks.length; k++) {
    const cmd = marks[k][1]
    const from = marks[k].index + marks[k][0].length
    const to = k + 1 < marks.length ? marks[k + 1].index : from + 2400
    const body = clean.slice(from, to)
    const required = new Set()
    const optional = new Set()
    const aliases = new Map() // alias -> the canonical name it stands for
    let unreadStruct = false
    for (const a of body.matchAll(/arg_(str|i64|bool|f64|usize|opt_str)\(&args,\s*"([^"]+)"(?:,\s*"([^"]+)")?/g)) {
      if (a[1] === 'opt_str') {
        optional.add(a[2])
        if (a[3]) {
          optional.add(a[3])
          aliases.set(a[3], a[2])
        }
      } else required.add(a[2])
    }
    for (const a of body.matchAll(/\.get\(\s*"([^"]+)"/g)) optional.add(a[1])
    for (const a of body.matchAll(/args\[\s*"([^"]+)"\s*\]/g)) optional.add(a[1])
    for (const a of body.matchAll(/:\s*(?:[\w:]*::)?([A-Z][A-Za-z0-9_]*)\s*=\s*serde_json::from_value/g)) {
      const s = structs.get(a[1])
      if (!s) {
        unreadStruct = true
        continue
      }
      if (s.renamed) {
        unreadStruct = true
        continue
      }
      for (const f of s.required) required.add(f)
      for (const f of s.optional) optional.add(f)
      for (const [al, f] of s.aliases ?? []) aliases.set(al, f)
    }
    out.set(cmd, { required, optional, aliases, unreadStruct })
  }
  return out
}

/* ----------------------------------- the shell ---------------------------------- */

/** Tauri commands, and the payload keys each one expects under its own `rename_all`.
 * A parameter that is not `Option<..>` is one the client has to send. */
function tauriCommands(src) {
  const out = new Map()
  const clean = stripRs(src)
  const re =
    /#\[tauri::command([^\]]*)\]\s*(?:pub\s+)?(?:async\s+)?fn\s+([a-z_][a-z0-9_]*)\s*\(([\s\S]*?)\)\s*(?:->[\s\S]*?)?\s*\{/g
  let m
  while ((m = re.exec(clean))) {
    const [, attr, cmd, params] = m
    const snake = /rename_all\s*=\s*"snake_case"/.test(attr)
    const parts = []
    let depth = 0
    let cur = ''
    for (const ch of params) {
      // `>` closes a generic. Leaving it out is not cosmetic: `state: tauri::State<'_, AppState>`
      // opens a depth that never comes back down, so every parameter after it folds into the same
      // fragment, only `state` gets recognised, and the command ends up demanding nothing at all —
      // which is how a two-word argument spelled in the wrong dialect reached a person's screen.
      if ('(<['.includes(ch)) depth++
      else if (')>]'.includes(ch) && depth > 0) depth--
      if (ch === ',' && depth === 0) {
        parts.push(cur)
        cur = ''
      } else cur += ch
    }
    parts.push(cur)
    const required = new Set()
    const optional = new Set()
    for (const p of parts) {
      const pm = p.trim().match(/^([a-z_][a-z0-9_]*)\s*:\s*([\s\S]*)$/)
      if (!pm) continue
      const [, name, ty] = pm
      if (['state', 'app', 'window', 'webview', 'manager', 'config'].includes(name)) continue
      const key = snake ? name : camel(name)
      if (/^Option\s*</.test(ty.trim())) optional.add(key)
      else required.add(key)
    }
    out.set(cmd, { required, optional, aliases: new Map(), unreadStruct: false, snake })
  }
  return out
}

/* ----------------------------------- the verdict --------------------------------- */

const { calls, skipped, sites, unread } = clientCalls(read(UI))
const structs = structFields(ENGINE_FILES)
const gw = gatewayArms(read(GW), structs)
const tv = tauriCommands(read(TAURI))

const bad = []
const vague = []
let hostChecks = 0
for (const [cmd, keys] of calls) {
  if (cmd === 'attach') continue // the transport rewrites this one's payload
  const hosts = [
    ['gateway', gw.get(cmd)],
    ['desktop', tv.get(cmd)],
  ].filter(([, h]) => h)
  if (!hosts.length) {
    bad.push(`${cmd}: neither the gateway dispatch nor a desktop command implements it — the page calls into the void`)
    continue
  }
  if (hosts.length === 1) {
    // One host answers and the other does not. Some of those are correct — a window cannot be
    // presented to a phone — but "correct" has to be a decision someone can look at, because the
    // other shape of this is a feature that works on a laptop and dies with `unknown verb` on a
    // phone. The client cannot be blamed for the spelling here: there is nothing to spell. So it is
    // named, and not failed.
    const only = hosts[0][0]
    const other = only === 'gateway' ? 'desktop' : 'gateway'
    vague.push(`${cmd}: ${other} has no such verb — a ${other === 'desktop' ? 'phone or browser' : 'desktop window'} asking the same question gets "unknown verb"`)
  }
  for (const [where, h] of hosts) {
    hostChecks++
    if (h.unreadStruct) vague.push(`${cmd} (${where}): argument shape comes from a type this script could not read`)
    for (const need of h.required) {
      if (keys.has(need)) continue
      const aliasOf = h.aliases.get(need)
      if (aliasOf && keys.has(aliasOf)) continue
      const near = [...keys].filter((k) => camel(k) === need || need === camel(k))
      if (near.length) {
        bad.push(
          `${cmd} (${where}): needs \`${need}\` and the client spells it \`${near.join('`, `')}\` — ` +
            `one host reads it, this one answers "missing required key ${need}"`,
        )
      } else {
        bad.push(
          `${cmd} (${where}): needs \`${need}\` and the client never sends it (it sends: ${[...keys].join(', ') || 'nothing'})`,
        )
      }
    }
  }
}

console.log(
  `[wire-names] client: ${calls.size} verbs with arguments across ${sites} call site(s); hosts read: `
    + `${gw.size} gateway arms, ${tv.size} desktop commands; ${hostChecks} verb-host pairs graded` +
    (unread ? `; ${unread} call site(s) too dynamic to read` : '') +
    (skipped.length ? `; ${skipped.length} payload(s) not literal: ${skipped.join('; ')}` : ''),
)
if (unread) {
  console.log(
    `[wire-names] note: ${unread} invoke site(s) name their verb in a way this script cannot follow. ` +
      `Those calls are ungraded — a missing or misspelled argument there is a runtime error, not a ` +
      `report here. Name the verb as a literal if you can.`,
  )
}
for (const v of vague) console.log(`[wire-names] note: ${v}`)
if (bad.length) {
  for (const b of bad) console.error(`[wire-names] FAIL ${b}`)
  console.error(
    `[wire-names] WIRE NAME DRIFT (${bad.length}): one client, two hosts, and an argument name this ` +
      `host will not read. What the person sees is either "missing required key ..." naming a word ` +
      `they never typed, or a value they typed being quietly dropped.`,
  )
  process.exit(1)
}
console.log('[wire-names] OK: every argument a host demands is a name the client actually sends.')
