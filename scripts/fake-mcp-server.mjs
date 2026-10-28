#!/usr/bin/env node
// A fake MCP server for cwoffee's tests. Stdio, JSON-RPC 2.0, newline-delimited — the
// transport MCP actually speaks over a local pipe, with no dependency on anybody's SDK: a
// test helper that pulls in the official client library would fail when the library changes,
// and the thing under test is our framing, not theirs.
//
// What it can be told to do, by argv:
//   --tools N        how many tools to offer (default 3; names are t1..tN)
//   --die            answer tools/list, then close stdout: the "server died" case
//   --slow <ms>      stall that long on tools/call: the timeout case
//   --dialect <v>    answer initialize with a different protocolVersion
//   --no-list        never answer tools/list
//   --writes <dir>   let `write_note` really write, so a caller can see a file appear
//   --noise          print log lines on stdout between frames, which a reader that assumed
//                    "one line = one answer" would grade as a tool result
//
// Nothing here reaches a model or a network. It is a program that answers three methods.
import { writeFileSync } from 'node:fs'
import { join } from 'node:path'

const args = process.argv.slice(2)
const flag = (name) => {
  const i = args.indexOf(name)
  return i === -1 ? null : args[i + 1]
}
const has = (name) => args.includes(name)

const nTools = Number(flag('--tools') ?? 3)
const slowMs = Number(flag('--slow') ?? 0)
const dialect = flag('--dialect') ?? '2024-11-05'
const writeDir = flag('--writes')
const noise = has('--noise')
const die = has('--die')

const say = (obj) => process.stdout.write(JSON.stringify(obj) + '\n')
const note = (line) => {
  if (noise) process.stdout.write(line + '\n')
}

const tools = []
for (let i = 1; i <= nTools; i += 1) {
  tools.push({
    name: `t${i}`,
    description: `Echo back the text given to tool ${i}, for a test that can tell the tools apart.`,
    inputSchema: {
      type: 'object',
      properties: { text: { type: 'string', description: 'the words to echo' } },
      required: ['text'],
      additionalProperties: false,
    },
    // Half of them claim to be read-only, so the risk rule has two cases to grade: a claim
    // lowers the bar to "ask once per call", it never lifts it to "never ask".
    annotations: i % 2 === 1 ? { readOnlyHint: true } : {},
  })
}
if (writeDir) {
  tools.push({
    name: 'write_note',
    description: 'Write a note file inside the folder this server was given.',
    inputSchema: {
      type: 'object',
      properties: { name: { type: 'string' }, body: { type: 'string' } },
      required: ['name', 'body'],
      additionalProperties: false,
    },
    annotations: {},
  })
}
// Offered so a caller can grade "the server said no" separately from "the server died": the
// first is a tool result with an error flag, the second is a broken pipe.
tools.push({
  name: 'boom',
  description: 'Always answers that the server refused. For the error-flag path.',
  inputSchema: { type: 'object', properties: {}, additionalProperties: false },
  annotations: {},
})

// A name no provider guarantees. It has to be refused at the catalog, not sanitized into
// something the model was never told about.
tools.push({
  name: 'not a valid name',
  description: 'Offered with a name cwoffee must refuse.',
  inputSchema: { type: 'object', properties: {}, additionalProperties: false },
})

let id = 0
const lines = []
let buffered = ''

process.stdin.setEncoding('utf8')
process.stdin.on('data', async (chunk) => {
  buffered += chunk
  let cut
  while ((cut = buffered.indexOf('\n')) >= 0) {
    const line = buffered.slice(0, cut)
    buffered = buffered.slice(cut + 1)
    if (line.trim()) await handle(JSON.parse(line))
  }
})

async function handle(msg) {
  id += 1
  if (msg.method === 'initialize') {
    note('[fake-mcp] initializing')
    say({
      jsonrpc: '2.0',
      id: msg.id,
      result: {
        protocolVersion: dialect,
        capabilities: { tools: {} },
        serverInfo: { name: 'fake-mcp', version: '0.0.1' },
      },
    })
    return
  }
  if (msg.method === 'notifications/initialized') return
  if (msg.method === 'tools/list') {
    say({ jsonrpc: '2.0', id: msg.id, result: { tools } })
    if (die) {
      // Exit right after the list, with no timer: the catalog is real and the *call* into it is
      // the thing that fails. A timer would let a fast caller win a race, and a test that wins
      // by being fast proves nothing.
      process.exit(0)
    }
    return
  }
  if (msg.method === 'tools/call') {
    const name = msg.params?.name
    if (slowMs > 0) await new Promise((r) => setTimeout(r, slowMs))
    if (name === 'write_note' && writeDir) {
      const path = join(writeDir, String(msg.params?.arguments?.name ?? 'note.txt'))
      writeFileSync(path, String(msg.params?.arguments?.body ?? ''))
      say({
        jsonrpc: '2.0',
        id: msg.id,
        result: { content: [{ type: 'text', text: `wrote ${path}` }], isError: false },
      })
      return
    }
    if (name === 'boom') {
      say({
        jsonrpc: '2.0',
        id: msg.id,
        result: { content: [{ type: 'text', text: 'the server refused to do that' }], isError: true },
      })
      return
    }
    if (!tools.some((t) => t.name === name)) {
      say({
        jsonrpc: '2.0',
        id: msg.id,
        error: { code: -32601, message: `unknown tool ${name}` },
      })
      return
    }
    const text = msg.params?.arguments?.text ?? ''
    say({
      jsonrpc: '2.0',
      id: msg.id,
      result: {
        content: [
          { type: 'text', text },
          // A block cwoffee cannot render as text, so the answer has to say it dropped
          // something rather than silently returning only the text half.
          { type: 'image', data: 'aGVsbG8=', mimeType: 'image/png' },
        ],
        isError: false,
      },
    })
    return
  }
  if (msg.id !== undefined) {
    say({ jsonrpc: '2.0', id: msg.id, error: { code: -32601, message: `no method ${msg.method}` } })
  }
}

process.stdin.on('end', () => process.exit(0))
id = 0
