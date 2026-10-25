#!/usr/bin/env node
// Does "keep devices working after this window closes" actually leave a gateway listening?
//
//   node scripts/gateway-handoff.mjs                 # rust core, loopback bind
//   node scripts/gateway-handoff.mjs --core pi       # pi core (needs target/release/cwtool)
//   node scripts/gateway-handoff.mjs --lan           # bind 0.0.0.0, no token anywhere
//
// Why this is a script and not a unit test: the claim is about processes. The gateway inside the
// desktop app is a task in the app's own process, so "keep it running" can only mean a second
// process that owns its own engine and reads the same config, store and token. Three things can
// go wrong and none of them is visible from inside the app: the standalone binary is not there,
// the child is killed with the app's systemd scope, or the child cannot authenticate because the
// token never left the first process.
//
// So the checks are made from outside, after the app has exited:
//   * the app's own hand-off line says it bound the port (it probes, it does not assume);
//   * the app process is gone;
//   * the port still answers, and a websocket on it authenticates with the token file the app
//     wrote — which is the part a hand-off could quietly lose, since the token is deliberately
//     not passed on a command line where any process on the box could read it in `ps`;
//   * the gateway answers a real command (`session_state`), so it is a working gateway and not a
//     socket somebody left open.
//
// Scratch everything: config, data, state (the hand-off log lives there), workspace, pi home,
// mock endpoint, gateway port. Nothing here touches ~/.config/cwoffee, sessions.db or :8090, and
// the endpoint is scripts/mock-llm.mjs — a test has no right to spend the user's prompt cache.
import { execFileSync, spawn } from 'node:child_process'
import net from 'node:net'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const here = path.dirname(fileURLToPath(import.meta.url))
const repo = path.resolve(here, '..')
const args = process.argv.slice(2)
const flag = (n, d) => {
  const i = args.indexOf(n)
  return i >= 0 && args[i + 1] ? args[i + 1] : d
}
const CORE = flag('--core', process.env.CWOFFEE_CORE ?? 'rust')
// Which window to hand off. The installed launcher runs a *copy* of the binary under
// ~/.local/share/cwoffee-desktop, and that copy has to find its own gateway sibling — pointing
// this script at it is how "the setting works from the apps menu path" gets checked rather than
// assumed.
const APP = flag('--app', path.join(repo, 'target/release/cwoffee-app'))
const LAN = args.includes('--lan')
// Negative control: hand the app a *smaller* environment and watch the check notice. Without
// this, "the unit's environment carries the extension" is a line printed, not a test with teeth.
const OmitEnv = args.includes('--omit-ext') ? ['CWOFFEE_PI_EXT', 'CWOFFEE_CWTOOL'] : []
const MOCK_PORT = +flag('--mock-port', '8191')
const GW_PORT = +flag('--gw-port', '8192')
// A failed check throws rather than printing and carrying on. The first version printed the
// failure and then kept running the steps after it, so a red line appeared above a green one and
// the run "passed" its own summary - and it left the gateway it had started holding the port,
// which breaks the next run. One exit path, and it always stops what it started.
const die = (m) => {
  throw new Error(m)
}

const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'cw-handoff-'))
const envFor = (dir) => ({
  ...process.env,
  XDG_CONFIG_HOME: path.join(scratch, 'config'),
  XDG_DATA_HOME: path.join(scratch, 'data'),
  XDG_STATE_HOME: path.join(scratch, 'state'),
  CWOFFEE_WORKSPACE: path.join(scratch, 'ws'),
})
for (const d of ['config', 'data', 'state', 'ws', 'pi']) fs.mkdirSync(path.join(scratch, d), { recursive: true })
fs.mkdirSync(path.join(scratch, 'config/cwoffee'), { recursive: true })
fs.writeFileSync(
  path.join(scratch, 'config/cwoffee/config.toml'),
  `mode = "auto"\n\n[provider]\nkind = "openai"\nbase_url = "http://127.0.0.1:${MOCK_PORT}/v1"\nmodel = "cwoffee-mock"\napi_key = "unused"\n`
)

// The gateway token: deliberately not in the environment and not in argv. The app writes the
// file when it turns remote access on; the hand-off's child has to find that same file.
const tokenFile = path.join(scratch, 'config/cwoffee/gateway.token')
delete envFor(scratch).CWOFFEE_GATEWAY_TOKEN

const cleanup = []
// Every step gets its own deadline. The first version of this awaited `child.on('exit')` on a
// process that had already exited, which never fires again, and node reported "unsettled
// top-level await" instead of a result - a checker that cannot finish is a checker that failed.
const withTimeout = (p, ms, what) =>
  Promise.race([p, new Promise((res) => setTimeout(res, ms, `${what}: no reply after ${ms}ms`))])
const stopAll = async (code) => {
  for (const c of cleanup.reverse()) await withTimeout(Promise.resolve().then(c), 2000, 'cleanup')
  if (code === 0) fs.rmSync(scratch, { recursive: true, force: true })
  else console.log(`[36m[handoff][0m left in place for diagnosis: ${scratch}`)
  process.exit(code)
}

/** Is something already on this port? If it is, it is not mine, and testing against it would
    grade somebody else's gateway. */
const portFree = (port) =>
  new Promise((res) => {
    const s = net.connect(port, '127.0.0.1')
    s.once('connect', () => (s.destroy(), res(false)))
    s.once('error', () => res(true))
    setTimeout(() => (s.destroy(), res(true)), 800)
  })


const say = (m) => console.log(`\x1b[36m[handoff]\x1b[0m ${m}`)

async function main() {
  for (const p of [MOCK_PORT, GW_PORT]) {
    if (!(await portFree(p))) die(`port ${p} is already in use. This script grades a gateway it starts itself; point it elsewhere with --mock-port/--gw-port.`)
  }

  // 1. the scripted endpoint
  const mock = spawn('node', [path.join(repo, 'scripts/mock-llm.mjs'), '--port', String(MOCK_PORT)], {
    stdio: ['ignore', 'ignore', 'inherit'],
    env: envFor(scratch),
  })
  cleanup.push(() => new Promise((r) => (mock.on('exit', r), mock.kill('SIGTERM'))))
  await new Promise((res, rej) => {
    const t = setTimeout(() => rej(new Error('mock endpoint did not come up')), 5000)
    const iv = setInterval(() => {
      const s = net.connect(MOCK_PORT, '127.0.0.1', () => (clearInterval(iv), clearTimeout(t), s.destroy(), res()))
      s.on('error', () => {})
    }, 100)
  })
  say(`mock endpoint on :${MOCK_PORT}; gateway port ${GW_PORT}; core ${CORE}; bind ${LAN ? '0.0.0.0' : '127.0.0.1'}`)
  say(`window: ${APP}`)

  // 2. the app, told to hand the gateway over and exit without opening a window
  const appEnv = {
    ...envFor(scratch),
    CWOFFEE_CORE: CORE,
    CWOFFEE_GATEWAY_PORT: String(GW_PORT),
    CWOFFEE_GATEWAY_HOST: LAN ? '0.0.0.0' : '127.0.0.1',
  }
  if (CORE === 'pi') {
    appEnv.CWOFFEE_PI_EXT = path.join(repo, 'pi-tools/cwoffee-tools.ts')
    appEnv.CWOFFEE_CWTOOL = path.join(repo, 'target/release/cwtool')
    for (const k of OmitEnv) delete appEnv[k]
    appEnv.CWOFFEE_PI_AGENT_DIR = path.join(scratch, 'pi')
    appEnv.CWOFFEE_PI_SESSION_DIR = path.join(scratch, 'pi/sessions')
    if (OmitEnv.length) say(`negative control: ${OmitEnv.join(', ')} deliberately not in the environment`)
    if (!OmitEnv.length && !fs.existsSync(appEnv.CWOFFEE_CWTOOL)) {
      die(`the pi core needs a tool host and ${appEnv.CWOFFEE_CWTOOL} is missing (cargo build --release -p cwtool)`)
    }
  }
  const app = spawn(
    path.join(repo, 'target/release/cwoffee-app'),
    ['--handoff-gateway', '--url', `http://127.0.0.1:${MOCK_PORT}/v1`, '--model', 'cwoffee-mock', '-C', path.join(scratch, 'ws')],
    { stdio: ['ignore', 'pipe', 'pipe'], env: appEnv }
  )
  let appOut = ''
  app.stderr.on('data', (b) => (appOut += b.toString()))
  app.stdout.on('data', (b) => (appOut += b.toString()))
  const appExit = new Promise((res) => app.on('exit', (code, sig) => res({ code, sig })))
  // Whatever happens from here, the gateway that gets started must not outlive the run: it holds
  // the port, and a leftover unit makes the next run's first check a lie. Registered before the
  // hand-off line is even read, because the failure that leaves one behind is a failure that
  // happens while reading it.
  cleanup.push(() => {
    try {
      execFileSync('systemctl', ['--user', 'stop', `cwoffee-gateway-${GW_PORT}`], { stdio: 'ignore' })
    } catch {
      /* no such unit */
    }
    try {
      execFileSync('pkill', ['-f', `cwoffee-gateway.*--port ${GW_PORT}`], { stdio: 'ignore' })
    } catch {
      /* nothing to kill */
    }
    return Promise.resolve()
  })
  cleanup.push(() => (app.exitCode === null ? new Promise((r) => (app.on('exit', r), app.kill('SIGKILL'))) : Promise.resolve()))

  // 3. the app's own verdict, then its own exit
  const line = await new Promise((res, rej) => {
    const t = setTimeout(() => rej(new Error(`no hand-off line within 60s.\n${appOut}`)), 60000)
    const iv = setInterval(() => {
      const m = appOut.match(/\[handoff\] bound=(\w+) pid=(\d+) via=(\S+) binary=(\S+) stop=`([^`]*)`/)
      if (m) (clearInterval(iv), clearTimeout(t), res(m))
      if (app.exitCode !== null && appOut.includes('[handoff] failed')) {
        (clearInterval(iv), clearTimeout(t), rej(new Error(appOut.trim())))
      }
    }, 100)
  })
  const [, bound, childPid, via, binary, stopCmd] = line
  say(`the app says: bound=${bound} via=${via} binary=${binary}`)
  say(`the app says: ${appOut.split('\n').find((l) => l.startsWith('[handoff] '))?.replace('[handoff] ', '') ?? ''}`)
  const gone = await Promise.race([appExit, new Promise((r) => setTimeout(() => r(null), 8000))])
  if (!gone) die(`the app did not exit after the hand-off (pid ${app.pid} still up)`)
  if (gone.code !== 0) die(`the app exited ${gone.code}: ${appOut.trim()}`)
  say(`the app is gone (exit ${gone.code})`)
  if (bound !== 'true') die(`the app itself reported it did not bind the port:\n${appOut}`)

  // 4. the promise: someone still answers on that port, with the token the app wrote
  const connect = () =>
    new Promise((res, rej) => {
      const s = net.connect(GW_PORT, '127.0.0.1')
      s.once('connect', () => (s.destroy(), res()))
      s.once('error', rej)
      setTimeout(() => (s.destroy(), rej(new Error('connect timeout'))), 1500)
    })
  try {
    await connect()
  } catch (e) {
    const log = path.join(scratch, 'state/cwoffee/gateway-handoff.log')
    const tail = fs.existsSync(log) ? fs.readFileSync(log, 'utf8').split('\n').slice(-14).join('\n') : '(no hand-off log)'
    die(`nobody answers on :${GW_PORT} after the app exited (${e.message}).\nthe gateway's own lines:\n${tail}`)
  }
  say(`:${GW_PORT} is still listening after the app exited`)
  if (fs.existsSync(tokenFile)) {
    say(`token file the app wrote: ${tokenFile} (${fs.readFileSync(tokenFile, 'utf8').trim().length} bytes)`)
  } else {
    die(`the app never wrote a token file: ${tokenFile}`)
  }

  // 5. and it is a working gateway: authenticate, then ask it a question only a host can answer
  const token = fs.readFileSync(tokenFile, 'utf8').trim()
  const ws = new WebSocket(`ws://127.0.0.1:${GW_PORT}/ws`)
  let seq = 0
  const pending = new Map()
  let authed
  const auth = new Promise((res, rej) => (authed = { res, rej }))
  ws.addEventListener('open', () => ws.send(JSON.stringify({ type: 'auth', token })))
  ws.addEventListener('message', (e) => {
    const v = JSON.parse(e.data)
    if (v.type === 'auth_ok') return authed.res(v.role ?? 'drive')
    if (v.type === 'auth_error') return authed.rej(new Error(String(v.error)))
    const p = pending.get(v.id)
    if (p) (pending.delete(v.id), p(v))
  })
  ws.addEventListener('error', (e) => authed.rej(new Error(e.message ?? 'ws error')))
  const call = (cmd, a = {}) =>
    new Promise((res, rej) => {
      const id = ++seq
      pending.set(id, (v) => (v.ok ? res(v.data) : rej(new Error(String(v.error)))))
      ws.send(JSON.stringify({ id, cmd, args: a }))
    })
  const role = await Promise.race([auth, new Promise((_, rej) => setTimeout(() => rej(new Error('no auth reply')), 8000))])
  say(`authenticated as ${role} with the token file, not a token from argv`)
  const st = await call('session_state', {})
  say(
    `the gateway answers session_state: core=${CORE}, model=${st.model ?? '?'}, workspace=${st.workspace ?? '?'}, persona=${st.persona ?? '?'}`
  )
  if (LAN && !st) die('a LAN hand-off authenticated but answered nothing')

  // The child's own side of the story, read from outside. Two claims are graded here, and the first
  // one was broken when this code ran for the first time:
  //   * the unit's environment. A systemd-run service gets the *user manager's* environment, not the
  //     app's, so the hand-off names the variables one by one with --setenv=. Without that the child
  //     reads ~/.config/cwoffee and ~/.local/share/cwoffee instead of the session it was handed, and
  //     on the pi core it comes up with no extension and no tool host — a gateway that answers every
  //     question with "tool not found" while looking healthy from the outside.
  //   * the hand-off log is not empty. The file descriptors this process opens are inherited by
  //     systemd-run, not by the unit it starts, so the child's lines went to the journal and the log
  //     the app points people at was a blank file.
  {
    const log = path.join(scratch, 'state/cwoffee/gateway-handoff.log')
    if (!fs.existsSync(log)) die(`the hand-off wrote no log at ${log}, so a child that fails leaves no trace`)
    const lines = fs
      .readFileSync(log, 'utf8')
      .split('\n')
      .map((l) => l.trim())
      .filter(Boolean)
    const up = lines.find((l) => l.includes('gateway:') && l.includes(`:${GW_PORT}`)) ?? ''
    if (!up)
      die(
        `the hand-off log at ${log} does not contain the child saying which port it took. Its lines:\n${lines.slice(0, 12).join('\n') || '(empty: the unit is not writing to the file)'}`,
      )
    say(`the child's own lines are in the log the app named: ${up}`)

    if (/systemd-run/.test(via)) {
      const unit = `cwoffee-gateway-${GW_PORT}`
      const shown = execFileSync('systemctl', ['--user', 'show', unit, '-p', 'Environment'], { encoding: 'utf8' }).trim()
      const env = shown.replace(/^Environment=/, '')
      for (const want of [
        `XDG_CONFIG_HOME=${path.join(scratch, 'config')}`,
        `XDG_DATA_HOME=${path.join(scratch, 'data')}`,
        `XDG_STATE_HOME=${path.join(scratch, 'state')}`,
      ]) {
        if (!env.includes(want))
          die(`the unit was started without ${want}, so the gateway would serve a different session than the window it came from. ${unit}: ${env || '(no Environment property)'}`)
      }
      if (CORE === 'pi') {
        for (const want of ['CWOFFEE_PI_EXT=', 'CWOFFEE_CWTOOL=']) {
          if (!env.includes(want))
            die(`the pi hand-off unit has no ${want}, so the mounted tools would be missing: ${env || '(no Environment property)'}`)
        }
      }
      if (/TOKEN|SECRET|_KEY/.test(env)) die(`a credential travelled in the unit's environment, where it belongs in a file instead: ${env}`)
      say(`the unit's environment carries this session's config, data and state${CORE === 'pi' ? ', plus the pi extension and the tool host' : ''} — and no credential`)
    }
  }

  // 6. stop what we started, and prove it can be stopped
  say(`stopping it with the sentence the app printed: ${stopCmd}`)
  await new Promise((res) => {
    const parts = stopCmd.split(/\s+/)
    const p = spawn(parts[0], parts.slice(1), { stdio: 'ignore' })
    p.on('exit', res)
    p.on('error', res)
  })
  await new Promise((r) => setTimeout(r, 800))
  let still = false
  try {
    await connect()
    still = true
  } catch {}
  if (still) die(`the stop sentence the app printed did not stop it: ${stopCmd}`)
  say(`the port is closed again: ${stopCmd} works`)
  if (childPid) say(`(the child was pid ${childPid}; a different process from the window, which is the whole point)`)

  say("what this does not prove: a turn run through the handed-off gateway. scripts/gateway-ask.mjs covers a turn, on a gateway it starts itself.")
  console.log(`\nHANDOFF OK (${CORE}${LAN ? ', lan bind' : ', loopback'}): the gateway outlived the window, authenticated from the token file, and answered a command.`)
}

// One summary line, then the processes go away whatever happened.
main()
  .then(() => stopAll(0))
  .catch(async (e) => {
    console.error(`\x1b[31m[handoff]\x1b[0m ${e instanceof Error ? e.message : e}`)
    await stopAll(1)
  })
