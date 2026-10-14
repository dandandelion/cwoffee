// Seed one real conversation through the gateway, so `gateway-smoke.mjs` is not
// reading an empty store. Same script for both cores: it is a client, not a test.
const port = process.argv[2] ?? '8101'
const token = process.argv[3] ?? 'SMOKE-TOKEN-123'
const ws = new WebSocket(`ws://127.0.0.1:${port}/ws`)
let sid = 0
const rpc = (id, cmd, args = {}) => ws.send(JSON.stringify({ id, cmd, args }))
const t = (ms) => new Promise((r) => setTimeout(r, ms))

ws.addEventListener('open', () => ws.send(JSON.stringify({ type: 'auth', token })))
ws.addEventListener('message', (ev) => {
  const v = JSON.parse(ev.data)
  if (v.type === 'auth_ok') rpc(1, 'new_session', { title: 'smoke seed', workspace: '/tmp/cw-smoke-ws' })
  if (v.ok === true && v.id === 1) {
    sid = v.data
    console.log(`seed: session ${sid} created`)
    rpc(2, 'start_turn', { text: 'Count out loud from one to fifteen', session: sid })
  }
})
await t(4000)
ws.send(JSON.stringify({ id: 3, cmd: 'session_messages', args: { id: sid } }))
await new Promise((res) => {
  ws.addEventListener('message', (ev) => {
    const v = JSON.parse(ev.data)
    if (v.ok === true && v.id === 3) {
      const rows = v.data?.messages ?? v.data ?? []
      console.log(`seed: transcript rows after the turn = ${Array.isArray(rows) ? rows.length : '?'}`)
      res()
    }
  })
})
ws.close()
