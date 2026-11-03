#!/usr/bin/env node
// The terminal gets graded, for the first time.
//
// Nothing in this repo's suite used to touch `crates/tui`: 1194 lines of skin, 18 keybindings and
// two host commands were shipped on the strength of "it shares the engine". The engine had grown
// branch, tree, files ledger, revert and MCP, and the terminal had grown no doors for any of it —
// while `checkpoint_store: true` meant every TUI turn was copying files the terminal user could
// neither list nor put back. Bytes with no door, and no check that could notice.
//
// So this gate drives the **built binary** over a pty (tmux's own server, on a socket named for
// this run, so it cannot disturb the user's) against `scripts/mock-llm.mjs`. It never touches
// `http://127.0.0.1:8090`.
//
// What it grades is the **disk and the store**, not the screen:
//
//   * a turn writes a file, and the file holds the turn's words;
//   * `/files` names that path on the screen — the one thing read off the screen, because "does the
//     door show me the path" is a claim about the door, not about the bytes;
//   * `/undo`, confirmed with `y`, leaves the file holding the words it held *before* the turn —
//     read from the file, not from the line the terminal printed;
//   * `/branch` opens a second conversation, and the original's rows are byte-for-byte what they
//     were;
//   * a mid-turn press lands in the model's next request, and pressing Enter while an approval card
//     is up does not answer that card;
//   * `explore` with `[mcp]` empty says no server is running; with this run's own
//     `scripts/fake-mcp-server.mjs` it names a tool, and `/mcp` reports the hub's state — including
//     the server that answers the list and then dies, which is a sentence naming it.
//
// Run (through cargo-guard from selftest.sh, or by hand):
//   node scripts/tui-gate.mjs [--bin target/release/cwoffee] [--mock 8331]
//                             [--db /tmp/cwoffee-tui-gate] [--speed 1]
import { spawn } from 'node:child_process'
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const REPO = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const argv = process.argv.slice(2)
const arg = (n, d) => {
  const i = argv.indexOf(`--${n}`)
  return i >= 0 ? argv[i + 1] : d
}
const BIN = path.resolve(arg('bin', path.join(REPO, 'target/release/cwoffee')))
const MPORT = Number(arg('mock', 8331))
const SPEED = arg('speed', '1')
const DIR = path.resolve(arg('db', '/tmp/cwoffee-tui-gate'))
const t = (ms) => new Promise((r) => setTimeout(r, ms))

const NOTE = 'tui_gate_note.txt'
const BEFORE = 'the words that were there before\n'
const DURING = 'the words the turn wrote\n'

const fails = []
const notes = []
const ok = (s) => notes.push(s)
const bad = (s) => fails.push(s)

function sh(cmd, args, opts = {}) {
  return new Promise((resolve, reject) => {
    const p = spawn(cmd, args, { ...opts, stdio: ['ignore', 'pipe', 'pipe'] })
    let out = ''
    let err = ''
    p.stdout.on('data', (d) => (out += d))
    p.stderr.on('data', (d) => (err += d))
    p.on('error', reject)
    p.on('close', (code) =>
      code === 0 ? resolve(out) : reject(new Error(`${cmd} ${args.join(' ')} exited ${code}: ${err || out}`)),
    )
  })
}

/** The pane's plain text, newest viewport first-hand: no escape codes, so a check reads words. */
async function screen() {
  return sh('tmux', ['-L', SOCK, 'capture-pane', '-p', '-S', '-200', '-t', WIN])
}

async function waitFor(what, ms, poll = 250) {
  const until = Date.now() + ms
  while (Date.now() < until) {
    const s = await screen()
    if (what(s)) return s
    await t(poll)
  }
  return null
}

/// tmux spells control keys `C-x`, not `ctrl-x`, and `send-keys` answers an unknown name by
/// typing it literally. That is not cosmetic: a gate that "pressed Ctrl-U" and got the characters
/// `ctrl-u` left those bytes in the composer, then watched the card's own key arrive with words in
/// the box and do nothing. The names are translated here so the mistake cannot come back.
const KEY_NAMES = { 'ctrl-u': 'C-u', 'ctrl-q': 'C-q', 'ctrl-c': 'C-c', 'ctrl-a': 'C-a', esc: 'Escape' }

async function keys(...k) {
  for (const one of k) {
    const parts = one.split(' ').map((p) => KEY_NAMES[p] ?? p)
    await sh('tmux', ['-L', SOCK, 'send-keys', '-t', WIN, ...parts])
  }
}

async function type(text) {
  await sh('tmux', ['-L', SOCK, 'send-keys', '-t', WIN, '-l', '--', text])
}

async function sendLine(text) {
  await type(text)
  await sh('tmux', ['-L', SOCK, 'send-keys', '-t', WIN, 'Enter'])
}

/** Poll the store until a value arrives. The terminal's rows are written as the turn ends, and
 * the answer's text is on the screen before the last write lands; a check that reads the store at
 * the first sight of the answer is reading a record mid-edit. */
async function untilSql(db, sql, want, ms = 12000) {
  const until = Date.now() + ms
  let got = ''
  while (Date.now() < until) {
    got = await sqlite(db, sql)
    if (got === want) return got
    await t(300)
  }
  return got
}

async function sqlite(db, sql) {
  const out = await sh('sqlite3', ['-readonly', db, sql])
  return out.trim()
}

/** Every row of one conversation, as one string. `/branch` must leave this value untouched. */
const rowsOf = (db, session) =>
  sqlite(db, `select group_concat(role || '|' || content || '#', '\n') from messages where session_id=${session} order by id;`)

/** Wait until the terminal says it is not inside a turn. Sending a door while a turn runs is
 * refused by design, so the gate has to wait for the turn instead of grading a refusal as a bug. */
async function idle(ms = 40000) {
  // Only the composer's own title line. The transcript quotes the landing receipt, which says
  // "into this turn" as a fact about a message that already landed, and reading that as "still
  // busy" would make the gate wait for a state the app is not in.
  return waitFor((s) => /^┌ ask - Enter sends/m.test(s), ms)
}

async function state() {
  try {
    return await (await fetch(`http://127.0.0.1:${MPORT}/mock/state`)).json()
  } catch {
    return {}
  }
}

fs.rmSync(DIR, { recursive: true, force: true })
for (const d of ['ws', 'config', 'data', 'state', 'pi']) fs.mkdirSync(path.join(DIR, d), { recursive: true })
fs.writeFileSync(path.join(DIR, 'ws', NOTE), BEFORE)
fs.writeFileSync(path.join(DIR, 'ws', 'README.md'), '# scratch\n\nNothing here is precious.\n')

const SOCK = `cwoffee-tui-gate-${process.pid}`
const WIN = 'gate'
const DB = path.join(DIR, 'data', 'gate.db')
const ENDPOINT = `http://127.0.0.1:${MPORT}/v1`

// A mock left over from another run answers with the scenarios it read at start-up. Reusing it
// silently would grade this gate against a script it never loaded — which is how one harness run
// reported a scenario that the server answering it had never seen. So: ask whether the port holds
// *this* file, reuse it only when it does, and end only a server this gate started, by its pid.
const crypto = await import('node:crypto')
const fingerprint = crypto
  .createHash('sha256')
  .update(fs.readFileSync(path.join(REPO, 'tests/scenarios/scenarios.json')))
  .digest('hex')
  .slice(0, 12)

async function whoAnswers() {
  try {
    const r = await fetch(`http://127.0.0.1:${MPORT}/mock/state`)
    return await r.json()
  } catch {
    return null
  }
}

let mock = null
let mockOut = ''
const existing = await whoAnswers()
if (existing && existing.fingerprint === fingerprint) {
  console.log(`[tui-gate] port ${MPORT} already answers as a mock-llm with the CURRENT scenarios.json (fingerprint ${fingerprint}) - reusing it, and not ending it`)
} else {
  if (existing) {
    const pid = (await sh('sh', ['-c', `ss -ltnp 'sport = :${MPORT}' 2>/dev/null | grep -o 'pid=[0-9]*' | head -1 | cut -d= -f2`]).catch(() => '')).trim()
    const why =
      `port ${MPORT} answers as a mock-llm with a stale scenario set (it says ${existing.fingerprint ?? 'no fingerprint'}, the file says ${fingerprint}). ` +
      `End that server by its pid (${pid || 'not found from ss'}) — do not pattern-kill by name, something else may answer to it — or pass --mock …`
    bad(why)
    throw new Error(why)
  }
  mock = spawn('node', [path.join(REPO, 'scripts/mock-llm.mjs'), '--port', String(MPORT), '--speed', String(SPEED)], {
    stdio: ['ignore', 'pipe', 'pipe'],
  })
  mock.stdout.on('data', (d) => (mockOut += d))
  mock.stderr.on('data', (d) => (mockOut += d))
  console.log(`[tui-gate] mock endpoint pid ${mock.pid} (ended by that number, never by a pattern)`)
}

const homes = {
  XDG_CONFIG_HOME: path.join(DIR, 'config'),
  XDG_DATA_HOME: path.join(DIR, 'data'),
  XDG_STATE_HOME: path.join(DIR, 'state'),
  CWOFFEE_PI_AGENT_DIR: path.join(DIR, 'pi'),
  // The scratch workspace, and no gateway port: the terminal opens no gateway at all.
  CWOFFEE_WORKSPACE: path.join(DIR, 'ws'),
  CWOFFEE_URL: ENDPOINT,
  CWOFFEE_MODEL: 'cwoffee-mock',
  PATH: process.env.PATH,
  HOME: process.env.HOME,
  LANG: process.env.LANG ?? 'C.UTF-8',
  TERM: 'xterm-256color',
  RUST_LOG: process.env.RUST_LOG ?? 'warn',
}

/** Shell-quote one value. The MCP JSON carries spaces, quotes and braces, and a tmux command
 * line is parsed by a shell: interpolating it into the pane's argv is how a config value arrives
 * cut in half, and a run that grades "no servers" because the env was mangled grades nothing. */
const sq = (v) => `'${String(v).replace(/'/g, `'\\''`)}'`

/**
 * Start the built terminal on a pty. `extra` are argv additions — today the palette arm's
 * `--config`, which is how the gate gives the terminal a *file* to read rather than an environment
 * variable: the claim being graded is that a stored value in `config.toml` reaches the chrome.
 */
async function launch(env, tag, extra = []) {
  const script = path.join(DIR, `run-${tag}.sh`)
  fs.writeFileSync(
    script,
    [
      '#!/bin/sh',
      // `env -i`, because tmux hands the pane whatever the *harness* exported. selftest.sh sets
      // CWOFFEE_MCP_SERVERS for its own run, and a run A that inherited it would be grading a
      // terminal with servers mounted while claiming to grade one without any.
      `exec env -i ${Object.entries({ ...homes, ...env })
        .map(([k, v]) => `${k}=${sq(v)}`)
        .join(' ')} ${sq(BIN)} --db ${sq(DB)} -C ${sq(path.join(DIR, 'ws'))} --url ${sq(ENDPOINT)} \
          --model cwoffee-mock --mode ask ${extra.join(' ')}`,
    ].join('\n'),
  )
  fs.chmodSync(script, 0o755)
  await sh('tmux', ['-L', SOCK, 'new-session', '-d', '-x', '220', '-y', '50', '-s', WIN, `sh ${sq(script)}`])
  const pid = (await sh('tmux', ['-L', SOCK, 'display-message', '-p', '-t', WIN, '#{pane_pid}'])).trim()
  console.log(`[tui-gate] ${tag}: terminal pid ${pid} on a tmux pty (socket ${SOCK}); killed with the server, not by name`)
  // The header is the first frame. A cold start opens sqlite and asks the endpoint for its
  // model list before it paints anything, so this is a boot allowance rather than a paint claim.
  const up = await waitFor((s) => /Cwoffee/.test(s), 60000)
  if (!up) bad('the built terminal never painted a header — the binary did not come up on a pty')
  return pid
}

let killed = false
async function killTerminal() {
  if (killed) return
  killed = true
  try {
    await sh('tmux', ['-L', SOCK, 'kill-server'])
  } catch (e) {
    // Nothing to end: the pane's process exited on its own (Ctrl-Q), which is the outcome the
    // gate wants. Say it once, not once per teardown call.
    ok(`tmux server ${SOCK} was already gone (${e.message})`)
  }
}

async function main() {
  if (!fs.existsSync(BIN)) {
    bad(`${BIN} does not exist — build it: cargo build --release -p cwoffee-tui (through scripts/cargo-guard.sh)`)
    return
  }
  for (let i = 0; i < 40; i++) {
    try {
      if ((await fetch(`http://127.0.0.1:${MPORT}/health`)).ok) break
    } catch {
      /* not up yet */
    }
    await t(200)
  }

  // ---- run A: no [mcp] servers at all ---------------------------------------
  const pidA = await launch({}, 'noservers')
  const scr0 = await screen()
  if (!/input|ask/.test(scr0)) bad('the composer never appeared; nothing below this line is graded')

  // A turn that really writes: the approval prompt is the terminal's own `ApprovalRequest`
  // render, answered with `y` rather than a mouse.
  await sendLine('TUI GATE: rewrite the note file')
  const card = await waitFor((s) => /APPROVAL REQUIRED/.test(s), 20000)
  if (!card) bad('write_file raised no approval card in the terminal — the one approval path it has is not working')
  else if (!/write_file/.test(card)) bad(`the approval card's headline is not the tool: ${card.match(/APPROVAL REQUIRED[^\n]*/)?.[0]}`)
  await keys('y')
  const done = await waitFor((s) => /TUI GATE DONE/.test(s), 30000)
  if (!done) bad('the turn never ended (no "TUI GATE DONE" on screen)')

  const wrote = fs.readFileSync(path.join(DIR, 'ws', NOTE), 'utf8')
  if (wrote !== DURING) bad(`the turn's write did not reach the disk: ${NOTE} holds ${JSON.stringify(wrote)}, expected ${JSON.stringify(DURING)}`)
  else ok(`the turn wrote ${NOTE} (${wrote.length}B)`)

  // The door. This is the screen's only job in the gate: does `/files` name the path?
  await sendLine('/files')
  const files = await waitFor((s) => /tui_gate_note\.txt/.test(s), 15000)
  if (!files) bad('/files printed no path — the terminal still cannot list the copies its turns took')
  else {
    // Read the listing's own block, not the screen. The tool card above it quotes the same path
    // in its arguments, and a check that matched that line would be grading the model's tool call
    // instead of the door.
    const at = files.split('\n').findIndex((l) => /files of conversation /.test(l))
    const block = at < 0 ? [] : files.split('\n').slice(at, at + 14)
    const line = block.find((l) => l.includes('tui_gate_note.txt')) ?? ''
    if (!/modified|created/.test(line))
      bad(`the ledger line does not carry the file's kind: ${JSON.stringify(line.trim().slice(0, 90))}${at < 0 ? ' (no "files of conversation" header on screen)' : ''}`)
    else ok(`/files names ${NOTE} as ${line.match(/(modified|created)/)[1]}`)
    if (!/index row|blob/.test(block.join('\n')))
      bad('/files lists paths but not what the copies cost; two numbers is the claim')
  }

  // `/undo`, and the bytes are the verdict.
  await sendLine('/undo')
  const plan = await waitFor((s) => /Revert that turn|Revert anyway/.test(s), 15000)
  if (!plan) bad('/undo asked for no confirmation — the terminal has a revert without a gate in front of it')
  else {
    await keys('y')
    const after = await waitFor((s) => /reverted row|refused:/.test(s), 15000)
    if (!after || /refused:/.test(after)) bad(`/undo's yes did not run: ${after?.match(/refused:[^\n]*/)?.[0] ?? 'nothing on screen'}`)
    const back = fs.readFileSync(path.join(DIR, 'ws', NOTE), 'utf8')
    if (back !== BEFORE) bad(`revert left ${NOTE} holding ${JSON.stringify(back)} instead of the words it held before (${JSON.stringify(BEFORE)})`)
    else ok('revert put the file back to its pre-turn bytes, read off the disk')
  }

  // `/branch`: a second conversation, and the first one's rows unchanged.
  await sendLine('TUI GATE: one sentence to branch from')
  // The person's own row is echoed the instant Enter lands, so matching its text says nothing about
  // the turn being over. The answer row's words and the idle composer are the two that do.
  if (!(await waitFor((s) => /TUI GATE BRANCHED-ANCHOR/.test(s), 30000))) bad('the anchor turn never answered')
  if (!(await idle())) bad('the terminal never reported the anchor turn finished: its composer still promises a mid-turn press')
  const before = await sqlite(DB, 'select count(*) from sessions;')
  const orig = await sqlite(DB, 'select max(id) from sessions;')
  const origRows = await rowsOf(DB, Number(orig))
  await sendLine('/branch')
  const branched = await waitFor((s) => /branched|refused:/.test(s), 15000)
  if (!branched || /refused:/.test(branched)) bad(`/branch did not open a conversation: ${branched?.match(/refused:[^\n]*/)?.[0] ?? 'nothing on screen'}`)
  else {
    const after = await sqlite(DB, 'select count(*) from sessions;')
    if (Number(after) !== Number(before) + 1) bad(`/branch made ${Number(after) - Number(before)} session row(s); expected exactly 1`)
    const now = await sqlite(DB, 'select max(id) from sessions;')
    if (now === orig) bad('/branch did not create a new conversation id — the host is still on the one it was on')
    const still = await rowsOf(DB, Number(orig))
    if (still !== origRows) bad('the original conversation changed: /branch is a copy, not an edit')
    else ok(`/branch copied conversation ${orig} into ${now} and left ${orig}'s rows identical (${origRows.split('\n').length} row(s))`)

    // The composer claim: the branch hands the words back rather than sending them, because what
    // to do with that message is the whole question the branch is asking. Read the input box's own
    // line — the transcript carries the same text above it, and matching that would prove nothing.
    const scr = await screen()
    const rows = scr.split('\n')
    const at = rows.findIndex((l) => /ask - Enter sends/.test(l))
    const box = at < 0 ? '' : (rows[at + 1] ?? '').trim()
    if (!box.includes('TUI GATE: one sentence to branch from'))
      bad(`/branch did not put the row's words back in the composer; the input line reads ${JSON.stringify(box.slice(0, 80))}`)
    else ok('/branch prefilled the composer with the row it cut from, and sent nothing')
  }
  // Ctrl-U empties the composer. Without it the prefilled words would ride along into the next
  // prompt, and the scenario the mock matches would be a different sentence.
  await keys('ctrl-u')

  // A mid-turn press beside a live approval card: the press queues, and it answers nothing. The
  // desktop's card has always worked that way (typing goes to the composer, only the button
  // resolves the decision); the terminal used to hand every keystroke to the card, which made the
  // queue unreachable while a card was up.
  if (!(await idle())) bad('still busy before the card case: the press below would be queued, not sent')
  await sendLine('Selftest steer: read outside the workspace while a card is up')
  const card2 = await waitFor((s) => /APPROVAL REQUIRED/.test(s), 20000)
  if (!card2) bad('the card scenario raised no approval card, so "a press cannot answer a card" has nothing to measure')
  else {
    if (!/read_file/.test(card2)) bad(`the card does not name the tool: ${card2.match(/APPROVAL REQUIRED[^\n]*/)?.[0]}`)
    await type('Selftest steer beside the card')
    await sh('tmux', ['-L', SOCK, 'send-keys', '-t', WIN, 'Enter'])
    const beside = await waitFor((s) => /queued into this turn|refused:/.test(s), 8000)
    if (!beside || /refused:/.test(beside))
      bad(`the press beside a card did not queue: ${beside?.match(/refused:[^\n]*/)?.[0] ?? 'nothing on screen'}`)
    const still = await screen()
    if (!/APPROVAL REQUIRED/.test(still)) bad('the mid-turn press answered the approval card — a typed message is not an answer for a person')
    else ok('a press queued beside the card and answered nothing: the card is still up')
    // Ctrl-U empties the composer, which is what makes the card's key mean its key again.
    await keys('ctrl-u')
    const emptied = await screen()
    if (/the composer holds words/.test(emptied)) bad('Ctrl-U did not empty the composer; the card key is still shadowed by typing')
    await keys('y')
    if (!(await waitFor((s) => /Read it after the card was answered/.test(s), 45000))) bad('the turn parked on the card never finished')
    if (!(await idle())) bad('the card turn ended with the composer still promising a mid-turn press')
    const st = await state()
    const hits = (st.steer_hits ?? []).filter((h) => h.includes('Selftest steer beside the card'))
    if (!hits.length) bad("the model never received the mid-turn message: /mock/state reports no steer hit for it")
    else ok(`the steer landed in the model's next request, after the tool result (${hits.length} hit)`)
    if ((st.steer_misses ?? []).length) bad(`a steer was required and missing: ${st.steer_misses.join('; ')}`)
    const rows = await untilSql(
      DB,
      "select count(*) from messages where role='user' and content like '%Selftest steer beside the card%';",
      '1',
    )
    if (rows !== '1') bad(`the transcript holds ${rows} row(s) carrying the press; exactly one user row is the claim`)
    else ok('the transcript grew by exactly one user row for the press')
    // The receipt is a transcript line, and the transcript scrolls internally: read it where it
    // was written rather than hoping the viewport still holds it after the answer streamed.
    await keys('PageUp', 'PageUp', 'PageUp')
    const scrolled = await screen()
    await keys('End')
    if (!/landed at your press/.test(scrolled) || !/went into this turn at step/.test(scrolled))
      bad(
        `no receipt said where the press landed (screen after scrolling: ${JSON.stringify(
          scrolled.split('\n').filter((l) => /landed|went into|waiting/.test(l)).slice(0, 3),
        )})`,
      )
    else ok('the receipt names the landing and the step that took it, stamped at the press')
  }

  // What steering does *not* unlock: a door asked for mid-turn is refused, in the engine's own
  // sentence, and Stop hands the waiting words back rather than losing them.
  if (!(await idle())) bad('still busy before the stop case')
  await sendLine('Selftest steer: answer slowly and get stopped')
  const busyScreen = await waitFor((s) => /┌ .*Steering|┌ .*Follow-up|┌ .*into this turn/m.test(s), 15000)
  if (!busyScreen) bad('the composer never announced a mid-turn press, so the refusal below has no state to be refused in')
  else {
    await type('Selftest steer words that should come back')
    await sh('tmux', ['-L', SOCK, 'send-keys', '-t', WIN, 'Enter'])
    if (!(await waitFor((s) => /queued into this turn|queued after this turn|refused:/.test(s), 8000)))
      bad('the mid-turn press reached neither the queue nor a refusal')
    await sendLine('/branch')
    const refused = await waitFor((s) => /refused: a turn is running/.test(s), 8000)
    if (!refused) bad('/branch while a turn runs was not refused — steering opened a door it must not open')
    else ok(`/branch mid-turn refuses in the engine's sentence: ${JSON.stringify(refused.match(/refused:[^\n]*/)?.[0]?.slice(0, 80))}`)
    const sessBefore = await sqlite(DB, 'select count(*) from sessions;')
    await keys('ctrl-c')
    const back = await waitFor((s) => /Its words are back in the composer|Their words are back in the composer/.test(s), 15000)
    if (!back) bad('Stop said nothing about what it returned; silent loss of typed words is the failure this feature was built against')
    else {
      const rows = back.split('\n')
      const at = rows.findIndex((l) => /┌ ask - Enter sends/.test(l))
      const box = at < 0 ? '' : (rows[at + 1] ?? '').trim()
      if (!box.includes('Selftest steer words that should come back'))
        bad(`Stop said the words came back but the composer holds ${JSON.stringify(box.slice(0, 60))}`)
      else ok('Stop handed the waiting words back into the composer, and said so')
      const after = await sqlite(DB, 'select count(*) from sessions;')
      if (after !== sessBefore) bad('the refused /branch created a conversation anyway — a refusal that acts is worse than no refusal')
      else ok('the refusal created nothing: the session count is unchanged')
    }
    await keys('ctrl-u')
  }

  // `explore mcp:` with nothing configured has to say so, in the engine's sentence.
  await sendLine('MCP NONE')
  if (!(await waitFor((s) => /MCP NONE DONE/.test(s), 45000))) bad('the no-servers explore turn never finished')
  // The tool's answer is a row in the store, which is where the claim lives: what the model was
  // given, not what the screen happened to be showing when the gate looked.
  const rowsA = await sqlite(DB, "select group_concat(content,'\n') from messages where role='tool';")
  if (!/No MCP server is running/.test(rowsA))
    bad(`explore with no servers answered ${JSON.stringify(rowsA.slice(0, 200))} instead of "No MCP server is running"`)
  else ok('explore with no servers answers with the honest sentence, read off the store')
  await keys('ctrl-q')
  await t(600)
  await killTerminal()
  ok(`run A (no servers) terminal pid ${pidA} ended with its tmux server`)

  // ---- run B: this run's own fake server, plus one that dies -----------------
  const servers = JSON.stringify({
    fake: {
      command: 'node',
      args: [path.join(REPO, 'scripts/fake-mcp-server.mjs'), '--tools', '3', '--writes', path.join(DIR, 'ws')],
      enabled: true,
    },
    dead: { command: 'node', args: [path.join(REPO, 'scripts/fake-mcp-server.mjs'), '--tools', '2', '--die'], enabled: true },
  })
  const pidB = await launch({ CWOFFEE_MCP_SERVERS: servers }, 'fakeserver')
  // The boot lines go to stderr before ratatui switches to the alternate screen, so the pane has
  // no way to show them afterwards. What the person can read is `/mcp`, graded below; this notes
  // the trade rather than claiming a thing the gate cannot see.
  notes.push('boot stderr is not observable through the pty (alternate screen); the hub state is graded from /mcp instead')
  await sendLine('TUI GATE MCP: look for a third-party tool')
  if (!(await waitFor((s) => /TUI GATE MCP DONE/.test(s), 50000))) bad('the live-server explore turn never finished')
  const rowsB = await sqlite(DB, "select group_concat(content,'\n') from messages where role='tool';")
  if (!/mcp__fake__/.test(rowsB))
    bad(`explore named no third-party tool with a live server running; the tool rows read ${JSON.stringify(rowsB.slice(0, 220))}`)
  else ok(`explore reached this run's own server: ${JSON.stringify(rowsB.match(/mcp__fake__[a-z_]*/)?.[0])} is in the answer the model was given`)
  if (!/MCP server fake/.test(rowsB)) bad('explore named a third-party tool without naming the server it came from')
  const stB = await state()
  if ((stB.misses ?? []).length) bad(`the mock's require strings went unsatisfied: ${stB.misses.join('; ')}`)
  await sendLine('/mcp')
  const hub = await waitFor((s) => /catalog:|No MCP server is running/.test(s), 15000)
  if (!hub || !/catalog:/.test(hub)) bad('/mcp reported no catalog — the status page does not read the hub it started')
  else {
    if (!/fake: live/.test(hub)) bad(`/mcp does not report the live server as live: ${JSON.stringify(hub.slice(-300))}`)
    else ok('/mcp reports the hub, its catalog cost, and the servers by name')
    // The dead half is the claim the boot lines used to carry, and the alternate screen eats
    // those: what a person can actually read is the status page, so that is what grades it.
    if (!/dead:/.test(hub)) bad('/mcp does not name the server that answered the list and then died')
    else ok(`the dead server is a sentence naming it: ${JSON.stringify(hub.match(/dead [^\n]*/)?.[0]?.slice(0, 90))}`)
    if (!/[0-9]+B against a [0-9]+B ceiling/.test(hub))
      bad('/mcp does not state the catalog block against its byte ceiling')
    else ok('the catalog cost is printed against the same ceiling the engine enforces')
  }
  await keys('ctrl-q')
  await t(600)
  await killTerminal()
  ok(`run B (fake + dead servers) terminal pid ${pidB} ended with its tmux server`)

  // ---- run C: the stored palette reaches the terminal's chrome ----------------
  // The prompt's claim is that the stored value is the default for *every* surface that can read
  // it, and the terminal is the surface nobody was reading it for. Graded two ways: the boot line
  // says how the palette was chosen, and the escape codes the header paints differ between the two
  // palettes — which is the only way a pty can be asked "did the colour change" without trusting a
  // description of it.
  const themeCfg = path.join(DIR, 'theme-config.toml')
  fs.writeFileSync(themeCfg, 'theme = "light"\n')
  // `--config` is the terminal's own door to the file: the claim under test is that a value in
  // `config.toml` reaches the chrome, so it is passed as a file and not as an env override.
  const pidC = await launch({}, 'palette', [`--config ${sq(themeCfg)}`])
  const paletteLine = await waitFor((s) => /Cwoffee (light|dark) /.test(s), 40000)
  if (!paletteLine) bad('the header never named a palette, so "the terminal follows the stored theme" has nothing to measure')
  else {
    const said = paletteLine.match(/Cwoffee (light|dark) /)?.[1]
    if (said !== 'light')
      bad(`config says theme = "light" and the terminal paints in ${said} — the stored value died in the file`)
    else ok('the terminal paints the palette the config file stores (theme = "light" → header says light)')
    const codes = await sh('tmux', ['-L', SOCK, 'capture-pane', '-p', '-e', '-t', WIN])
    if (!/\[48;5;4m/.test(codes))
      bad(`the light badge is not painted as its own colour; header line: ${JSON.stringify(codes.split('\n')[0]?.slice(0, 90))}`)
    else ok('the badge colour changes with the palette, read out of the escape codes rather than described')
  }
  const chosen = await waitFor((s) => /the stored choice says light/.test(s), 6000)
  if (!chosen) bad('the terminal never said how its palette was chosen; a boot fact nobody can read is a boot fact that never happened')
  else ok('the boot line says how the palette was chosen: "the stored choice says light"')
  await keys('ctrl-q')
  await t(500)
  await killTerminal()
  ok(`run C (theme = light) terminal pid ${pidC} ended with its tmux server`)

  const st = await state()
  ok(`mock endpoint: ${st.user_seen?.length ?? 0} user message(s), ${(st.steer_hits ?? []).length} steer hit(s), ${st.steer_misses?.length ?? 0} missing, ${st.steer_leaks?.length ?? 0} leaked`)
  if ((st.steer_leaks ?? []).length) bad(`a withdrawn steer reached the model: ${st.steer_leaks.join('; ')}`)
}

main()
  .catch((e) => bad(`the gate threw: ${e?.stack ?? e}`))
  .finally(async () => {
    try {
      await killTerminal()
    } catch {}
    if (mock) {
      console.log(`[tui-gate] ending mock endpoint pid ${mock.pid}`)
      mock.kill('SIGTERM')
    }
    for (const n of notes) console.log(`  ok: ${n}`)
    if (fails.length) {
      for (const f of fails) console.log(`  FAIL: ${f}`)
      console.log(`TUI GATE FAILED (${fails.length})`)
      process.exitCode = 1
    } else {
      console.log('TUI GATE ok')
    }
  })
