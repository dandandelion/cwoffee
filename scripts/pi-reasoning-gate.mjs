#!/usr/bin/env node
// Does pi agree that the model entry cwoffee writes can answer for a thinking level?
//
// This grades pi's behaviour, not cwoffee's reading of pi's docs, and it exists because of a
// report that looked like a settings bug: "changing reasoning effort doesn't take effect".
//
// The chain that has to work: cwoffee's composer picks a level, the bridge asks pi to set it,
// pi clamps that level against what the *model entry* says it can do, and only a level that
// survives the clamp reaches the endpoint. cwoffee's entry used to carry `id`, `input`,
// `contextWindow` and `maxTokens` and nothing about thinking - and pi-ai's
// `getSupportedThinkingLevels` answers `["off"]` for any model whose entry does not say
// `reasoning: true`, with `clampThinkingLevel` mapping whatever was asked onto that. So every
// level was accepted, reported back to the app as applied, and never sent: a dial wired to
// nothing, in the one core the installed app actually runs.
//
// The fixture is the same file the Rust side pins (`mount::tests::
// the_pi_entry_declares_the_thinking_levels_the_rust_core_would_send`), so one document holds
// both halves: the Rust test grades what cwoffee writes, this script grades what pi makes of it.
//
// The negative control matters as much as the positive one: it asserts that the *old* entry
// shape really does clamp to off. If pi ever changes that, this gate fails and says so, rather
// than leaving a fix in place whose premise has quietly gone.
//
// Run: node scripts/pi-reasoning-gate.mjs            (PI_AI_MODELS=<path to pi-ai/dist/models.js> to point elsewhere)
import { spawnSync } from 'node:child_process'
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'

const REPO = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const FIXTURE = path.join(REPO, 'tests/fixtures/pi-model-entry.json')

function fail(msg) {
  console.log(`PI REASONING GATE FAIL: ${msg}`)
  process.exitCode = 1
}

/** Where pi-ai lives. Resolved from the installed pi rather than from this repo's node_modules,
 *  because the thing being graded is the runtime the app actually drives. */
function piAiDir() {
  if (process.env.PI_AI_DIR) return process.env.PI_AI_DIR
  const which = spawnSync('which', ['pi'], { encoding: 'utf8' }).stdout.trim()
  if (!which) throw new Error('no pi on PATH; set PI_AI_DIR to the pi-ai package directory')
  const cli = fs.realpathSync(which) // .../@earendil-works/pi-coding-agent/dist/bundle/cli.js
  const pkg = path.resolve(cli, '..', '..', '..')
  const ai = path.join(pkg, 'node_modules', '@earendil-works', 'pi-ai')
  if (!fs.existsSync(ai)) throw new Error(`pi-ai not where expected: ${ai}`)
  return ai
}

const ai = piAiDir()
const models = await import(pathToFileURL(path.join(ai, 'dist/models.js')).href)
const compatSchema = await import(pathToFileURL(path.join(ai, 'dist/providers/compat-schema.js')).href)
const frag = JSON.parse(fs.readFileSync(FIXTURE, 'utf8'))
const base = { id: 'qwen-on-this-box', provider: 'cwoffee', api: 'openai-completions' }

// ---- the negative control: the entry shape cwoffee used to write -------------------------
const filed = { ...base }
const offered = models.getSupportedThinkingLevels(filed)
if (offered.length !== 1 || offered[0] !== 'off') {
  fail(
    `the negative control no longer holds: an entry with no thinking fields offers ${JSON.stringify(offered)}, ` +
      'not ["off"]. Whatever this gate is protecting has changed on pi\'s side - re-read getSupportedThinkingLevels ' +
      'before trusting the fix that was built on it.',
  )
}
if (models.clampThinkingLevel(filed, 'high') !== 'off') {
  fail('the old entry shape no longer clamps `high` to off, so the premise of the fix needs re-checking')
} else {
  console.log('negative control ok (an entry without thinking fields offers only "off": the dial could not reach the wire)')
}

// ---- the entry cwoffee writes now -------------------------------------------------------
const declared = { ...base, ...frag }
const levels = models.getSupportedThinkingLevels(declared)
for (const want of ['low', 'medium', 'high', 'xhigh']) {
  if (!levels.includes(want)) fail(`pi offers ${JSON.stringify(levels)} for cwoffee's entry; ${want} is missing`)
}
for (const [ask, keep] of [['high', 'high'], ['xhigh', 'xhigh'], ['medium', 'medium'], ['off', 'off']]) {
  const got = models.clampThinkingLevel(declared, ask)
  if (got !== keep) fail(`pi clamped ${ask} to ${JSON.stringify(got)}; the entry promises it can answer for ${keep}`)
}
// A level the endpoint never offered stays closed - pi will not offer it either, which is the
// alternative to sending a value the server answers with a 400.
if (levels.includes('max')) fail('the entry claims a max level this endpoint never offered')

// ---- what the request will carry --------------------------------------------------------
// The values pi sends are taken from `thinkingLevelMap`, and the dialect is the one cwoffee's
// rust core speaks for the same kind of endpoint: `chat_template_kwargs.enable_thinking` plus
// `reasoning_effort`. Check the fragment against pi's own compat schema, so a shape pi accepts
// in a fixture but rejects in a models.json cannot slip through.
// pi's own compat schema, read as an object rather than validated with it: the claim is that
// every field name in the fixture is one pi declares, and that the dialect we ask for is one
// of the values pi enumerates. A field pi has never heard of is dropped in silence, which is
// how a thinking knob ends up doing nothing without any error anywhere.
const schema = compatSchema.OpenAICompletionsCompatSchema
const allowed = new Set(Object.keys(schema?.properties ?? {}))
const unknown = Object.keys(frag.compat ?? {}).filter((k) => !allowed.has(k))
if (unknown.length) {
  fail(`pi's compat schema has no field named ${unknown.join(', ')}; the entry would be read as noise`)
}
const dialects = (schema?.properties?.thinkingFormat?.anyOf ?? schema?.properties?.thinkingFormat?.union ?? [])
  .map((t) => t.const ?? t.pattern)
  .filter(Boolean)
if (dialects.length && !dialects.includes(frag.compat?.thinkingFormat)) {
  fail(`thinkingFormat ${JSON.stringify(frag.compat?.thinkingFormat)} is not one of pi's dialects: ${dialects.join(', ')}`)
}
if (frag.compat?.thinkingFormat !== 'chat-template') {
  fail('the dialect must be chat-template: that is the one that sends chat_template_kwargs')
}
const kwargs = frag.compat?.chatTemplateKwargs ?? {}
if (kwargs.enable_thinking?.$var !== 'thinking.enabled') {
  fail('enable_thinking has to follow whether thinking is on, or the template stays at its own default')
}
if (kwargs.reasoning_effort?.$var !== 'thinking.effort' || kwargs.reasoning_effort?.omitWhenOff !== true) {
  fail(
    'reasoning_effort must map the level and be omitted when thinking is off: the rust core sends ' +
      '{enable_thinking:false} with no effort in that case, and both cores must send one shape',
  )
}
// `high` folds to `xhigh` because that is the value this endpoint's template takes - the same
// folding `llama_effort` does in the rust provider, so a conversation thinks the same amount
// whichever core is behind the window.
if (frag.thinkingLevelMap?.high !== 'xhigh') {
  fail(`high maps to ${JSON.stringify(frag.thinkingLevelMap?.high)}; the template takes xhigh`)
}

if (!process.exitCode) {
  console.log(
    `pi reasoning gate ok (${JSON.stringify(levels)} offered for cwoffee's entry; high→${frag.thinkingLevelMap.high}, ` +
      'effort omitted when thinking is off; compat validates against pi\'s own schema)',
  )
}
