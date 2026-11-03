# Mid-turn steering: what you type during a turn, and where it goes

Until this feature, a press of Enter while cwoffee was answering changed nothing. The composer kept
the words, told the person once that Enter does not send during a turn, and waited. The engine's own
comment said why: `Send` runs its turn **inline** in the host's command loop, so a message sent
mid-turn sat in the mailbox until the answer finished — and then started a second turn. Pi already
had the capability (`steer`, `follow_up`, `clear_queue`, and the `queue_update` event that describes
the queue) and `crates/bridge/src/translate.rs` was swallowing `queue_update` with the pings, so
cwoffee threw it away and `docs/PI-CORE-PROTOCOL.md` filed the whole thing under "a feature the Rust
core never had".

Both cores have it now, and they answer the same gesture with the same words.

## Two promises, not one

`crate::engine::steer::MidTurn` is the whole vocabulary, and it is pi's:

| press | promise | where it lands |
|---|---|---|
| `steer` | into **this** turn | after the current step's tool results are back, before the next request |
| `follow_up` | **after** this turn | as the next thing the model is asked |

The distinction is not cosmetic. A card that calls a follow-up "steered into this turn" tells the
person the model is about to read their message inside a turn that will finish first. So every
sentence — the composer's announcement, the pending line, the landing receipt, the stop note — is one
function in `steer.rs`, used by both cores and by the gateway, and the page prints the host's strings
rather than paraphrasing them. `HostState` carries `mid_turn`, `mid_turn_hint`, `mid_turn_note` and
`mid_turn_refusal`; `QueuedView` carries `label` and `note`. A UI that wrote its own wording here
would be a second answer to "where is this message going", and the two drift the first time one of
them is edited.

## Where the queue lives, and why it is not a command

On the Rust core the mailbox cannot carry a mid-turn press: `SessionHost`'s loop runs the turn
inline, so a `HostCmd` sent during a turn is read *after* it — which is exactly the behaviour being
replaced. So the queue is a shared handle (`steer::Queue`, one `Arc<Mutex<…>>`) that the shell pushes
into and the agent polls at its step boundary. It is on the same footing as the cancel flag, which
had to be shared for the same reason: Stop arrived at the composer, not at the loop.

The boundary is one place in `agent.rs`, at the end of an iteration:

```rust
for call in &collected.calls { self.run_tool(call, out).await; }
if self.poll_queue(turn, iters + 1, out, session_id) > 0 { continue; }
```

… and again after a final answer, before the turn ends — because that is where pi polls too. pi's own
order is steering first, follow-ups only when nothing is steering and there are no more tool calls
(`pi-agent-core`'s README says exactly that), so `poll_queue` takes the same order and the run
continues rather than stopping. A steer typed while the model was writing its last sentence lands and
gets answered in the same run on both cores.

On the pi core the press *is* a command (`HostCmd::SteerQueued`), because that loop is a `select!`
and answers commands during a turn; it sends pi's `steer` or `follow_up` and the delivery arrives as
an echoed user record.

## Ids are ours; pi's queue has none

pi stores its queues as `string[]`. `_steeringMessages.indexOf(messageText)` is how it removes a
delivered message — the earliest match, which matters when the same words were typed twice. So:

* cwoffee assigns the id, at the press, on both cores. Withdrawal and the landing receipt are
  addressed by it, and `queue_state()` answers with the same ids a client was shown.
* `queue_update` carries the complete queue as text. The bridge reconciles its mirror against that,
  counting survivors **from the back** so the earliest occurrence is the one recorded as delivered.
* A text pi no longer lists is moved to `delivering` rather than dropped: pi removes the text at
  `message_start`, a beat before the `message_end` that cwoffee reads as the landing. Dropping early
  would match a duplicate text to the wrong message.

## Three rules the feature is built around

**Append-only, stamped when it was typed.** A landed press is pushed through
`PromptBuilder::push_user_stamped`, and `Message::sent_at` becomes the row's `created_at`. The stamp
is the moment Enter was pressed, so the transcript reads in the order the person typed rather than in
the order the loop got round to it. `sent_at` is `#[serde(skip)]`: a per-message clock in a request
body would change prompt bytes every turn and throw away the prefix cache. A volatile fact belongs in
the record, not in what the model is shown.

**Nothing is claimed early.** `SteerLanded` is emitted when the words go into the prompt the *next*
request is built from — never at the press. Before that, the pending line says "not sent to the model
yet", in the engine's words. A receipt that said the model has it before a request carried it would
be a claim about somebody's attention.

**A turn that stops gives the words back.** Stop — or a turn that ends in error — drains what is
still queued for that conversation and emits `QueueReturned`. pi documents exactly this for its own
Escape key: `clear_queue` before `abort`, then restore the returned text in the editor; `abort` alone
*continues* a run that still has queued messages, which is how a stopped turn would send something
the person had stopped. Each window puts back only the words **it** typed (`myQueue`), because the
event goes to every client and copying another device's message into this composer would be typing
something nobody typed here.

Silent loss is the failure the whole design is against.

## What steering does not unlock

The mid-turn refusals stay, unchanged: branch, leaf switch, workspace move, revert and forget refuse
while a turn runs; a steer cannot answer an approval card or a question, cannot change the approval
mode, and cannot change the tools the turn started with. `host.rs`'s sentence about a turn being
given its tools when it started is still true, and `mid_turn_refusal` is the same text the settings
page prints, so a refusal and a description cannot disagree.

And a landed message cannot be unsent. `landed_note` says so in the transcript: the bytes went out,
the row stays, and a "Unsend" control would be offering something no record can undo.

## The one setting

`mid_turn = "steer" | "follow_up"` in `config.toml`, on the host, so a phone tab and the desktop are
given the same promise for the same Enter. `MidTurn::parse` accepts `follow-up` and `followup` as
well; a word that is neither is refused at boot rather than silently becoming the default.

pi's delivery modes are settled rather than defaulted: the bridge sends
`set_steering_mode` and `set_follow_up_mode` with `all` (`steer::PI_QUEUE_MODE`) on every process
start, because cwoffee drains *every* pending message of a kind at one boundary. Left at pi's
`one-at-a-time` default, three presses during one tool batch would land one per turn on pi and all
three at the next step on the Rust core — one gesture, two answers, which is the drift the whole
`HostOps` seam exists to prevent. `one_boundary_drains_every_steer_because_pi_is_told_the_same_mode`
grades the Rust side and asserts the constant.

Withdrawal is where the two cores' shapes differ most honestly: the Rust core drops one item; pi has
no per-item removal, so `withdraw_queued_row` clears the queue and re-queues what the withdrawal did
not name, in typing order, rebuilding cwoffee's mirror from pi's returned text so the ids survive.
The cost of that shape is stated in the code: a press from another window that lands between the
clear and the re-queue is not in the returned text, and the next `queue_update` sorts the mirror out.

## What the checks grade

`./scripts/selftest.sh steer` runs four arms, and the load-bearing ones are graded by the scripted
endpoint rather than by the page:

- **the press reaches the model's next request, after the tool results and before the answer.**
  `steer-into-turn`'s second step carries `require_steer`, which `scripts/mock-llm.mjs` checks
  against user messages positioned *after the last tool result* of the request that plays it. A card
  that lied about a landing fails the turn with a 400 naming what the request actually held.
- **the transcript grew by exactly one user row** — read out of `session_messages`, not counted from
  a bubble.
- **a withdrawn press reaches neither the endpoint nor the record.** `steer-withdraw`'s second step
  carries `require_steer_absent`: if the words appear anywhere in that request, the endpoint refuses
  it, so "the card said it was taken back" cannot be the evidence.
- **Stop hands the words back and says so**, and **a press cannot answer a card** — the card stays up,
  the press queues beside it, and only the card's own button resolves it.

The card arm has a pi-core wrinkle worth knowing: which approval mode a pi process obeys is decided
when it is exec'd (`CWOFFEE_MODE` in the child's environment) and no RPC changes it afterwards. So the
arm relaunches the process through a persona round trip — the same door the `mode-tab` group uses —
and restores what it found in its `finally`.

`composer-type` was flipped with the feature rather than left beside it: its mid-turn assertions used
to prove a refusal ("Enter does not send while cwoffee is answering") and now prove the press — the
placeholder names the kind and where the words go, the box empties into a waiting line that says the
message has not been sent, no second turn starts, and the pressed words become a row of their own.
`checkpoint-slow` gained a second step because a steer makes that turn continue, and an exhausted
scenario would 400 on the request that carries it.

## One harness bug found on the way

`$path` in a scenario expands to the last path-shaped token of the prompt. The env block ends with
`</mcp>`, and a token with a slash looks path-shaped — so on a pi-core run with MCP servers,
`write_file` was handed `</mcp>` as its path. The fixture then wrote a file nobody read, the
`require` needles failed, and one run reported twelve failures across seven groups that shared that
one cause. `extractPath` now strips `<env>`, `<mcp>` and `<tools>` before looking, and refuses tokens
with angle brackets in them: markup is not a path.
