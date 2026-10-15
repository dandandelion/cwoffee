#!/usr/bin/env node
// A picture has to reach the *model*, not just the store.
//
//   node scripts/gateway-attach.mjs --port 8154 --token ATTOK
//
// Two ways this used to fail with nothing on screen: the gateway dropped
// `attachments` from `start_turn` on the floor, and pi silently omits image blocks when
// the model entry says nothing about modalities (`input`). Both look like a normal turn.
// So the assertion is made at the endpoint: the scripted model reports the image blocks
// it actually received, and the gate also checks that the stored row kept the client's
// own filename, which is what the transcript is supposed to show.
import { DatabaseSync } from 'node:sqlite'
const args = process.argv.slice(2)
const flag = (n, d) => { const i = args.indexOf(n); return i >= 0 && args[i + 1] ? args[i + 1] : d }
const PORT = +flag('--port', '8154')
const TOKEN = flag('--token', 'ATTOK')
const WORKSPACE = flag('--workspace', '/tmp/cw-smoke-ws')
const DB = flag('--db', '')
// A 1x1 PNG. It is a probe, not a photograph: the point is the bytes that travel.
const PNG =
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNgYGBgAAAABQABpfZFQAAAAABJRU5ErkJggg=='
const ws = new WebSocket(`ws://127.0.0.1:${PORT}/ws`)
let seq = 0
const pending = new Map()
let authed
const auth = new Promise((res, rej) => { authed = { res, rej } })
ws.addEventListener('open', () => ws.send(JSON.stringify({ type: 'auth', token: TOKEN })))
ws.addEventListener('message', (e) => {
  const v = JSON.parse(e.data)
  if (v.type === 'auth_ok') return authed.res(v.role ?? 'drive')
  if (v.type === 'auth_error') return authed.rej(new Error(String(v.error)))
  const p = pending.get(v.id)
  if (p) { pending.delete(v.id); p(v) }
})
ws.addEventListener('error', (e) => authed.rej(new Error(e.message ?? 'ws error')))
const call = (cmd, a = {}) => {
  const id = ++seq
  return new Promise((res, rej) => {
    pending.set(id, (v) => (v.ok ? res(v.data) : rej(new Error(String(v.error)))))
    ws.send(JSON.stringify({ id, cmd, args: a }))
    setTimeout(() => pending.has(id) && (pending.delete(id), rej(new Error('timeout ' + cmd))), 20_000)
  })
}
const notes = []
const fail = (m) => { console.error(`\nATTACH GATE FAILED: ${m}`); process.exit(1) }

;(async () => {
  await auth
  const sid = await call('new_session', { title: 'attachment reaches the model', workspace: WORKSPACE })
  ws.addEventListener('message', (e) => {
    const v = JSON.parse(e.data)
    if (v.type === 'event' && v.d?.kind === 'note') notes.push(String(v.d.d?.message ?? ''))
  })
  await call('start_turn', {
    text: 'ATTACHMENT ECHO',
    session: sid,
    attachments: [{ mime: 'image/png', data_b64: PNG, name: 'probe.png' }],
  })
  const page = await (async () => {
    for (let i = 0; i < 120; i++) {
      await new Promise((r) => setTimeout(r, 250))
      const p = await call('session_messages', { id: sid })
      const asst = (p.messages ?? []).filter((m) => m.role === 'assistant').map((m) => String(m.content ?? ''))
      if (asst.some((t) => t.includes('Attachment seen:'))) return p
    }
    fail(`the endpoint never reported the attachment; notes: ${JSON.stringify(notes)}`)
  })()
  const said = (page.messages ?? [])
    .filter((m) => m.role === 'assistant')
    .map((m) => String(m.content ?? ''))
  const receipt = said.find((t) => t.includes('Attachment seen:'))
  console.log('endpoint receipt:', JSON.stringify(receipt))
  if (!receipt.includes('1 image(s): image/png')) {
    fail(
      `the model was shown the wrong thing. 0 image(s) means a hop dropped it: the gateway does not \
forward attachments, or pi's model entry for this provider lacks "input": ["text","image"] and pi \
omitted the block before the request`,
    )
  }
  const kept = (page.messages ?? [])
    .flatMap((m) => m.attachments ?? [])
    .find((x) => x.name === 'probe.png')
  if (!kept) fail('the stored row did not keep the client\'s attachment name')
  console.log(`stored row keeps ${kept.name} (${kept.bytes ?? kept.data_b64?.length ?? '?'} bytes)`)
  console.log('\nATTACH GATE OK (bytes reached the model, and the transcript says what they were called)')
  process.exit(0)
})().catch((e) => fail(e.message))
