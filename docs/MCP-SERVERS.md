# Third-party MCP servers

Somebody else's program runs on this machine, and its tool schemas would be prompt bytes in
every turn. The cost question and the safety question are the same question here, and both are
answered by making third-party tools ride the seam cwoffee already has for its own: registered,
hidden, and named only by `explore`.

## The catalog, not the schemas

A server's tools are registered as **hidden** entries (`Registry::register_lazy`). Hidden means
two different things, and only one of them costs money:

* the tool is **callable** for the whole session — registering it is what makes it reachable;
* its **declaration** (name, description, JSON schema) is left out of the prompt until
  `explore` names it.

What is always in the prompt is the catalog: one line per server with its state and tool count,
and one line per tool with a truncated description. `CATALOG_BYTE_BUDGET` (1024 B) caps the tool
list; the fixed sentence explaining how to use it is counted separately, because the two numbers
answer different questions — what the prompt pays in total, and what grows when a server is
added. A list that hits the ceiling says how many tools it left out rather than presenting a
cut-off list as complete. `mcp_status` reports both numbers and the settings page prints them.

The catalog belongs to the words sent for a turn, not to the system prompt. Which servers are up
changes inside a session — one can be reloaded, one can die — and the stable prefix is the one
place a changing fact must not live. The Rust core folds the block into its `<env>` block on the
newest user message; on the pi core the extension re-reads it from the hub before every request
and appends it to the same message, so nothing stale is kept in the transcript.

## `explore` is the only door

`explore` with `need: "mcp: …"` searches the hidden tools, and its answer carries
`activates`. The host lifts the withholding for exactly those names, and the wording — "available
from your next call" — is kept literally true because the tools were callable all along and the
schemas arrive with the next request. Narrowing the *active* set instead would break that
promise: an inactive tool is not callable until the next agent turn, which is precisely the turn
the discovery happened in.

## Every call asks

* A tool from a server is never `Risk::Harmless`. A server's own `readOnlyHint` is the only
  downgrade taken, and it goes to `Workspace`: a third party's claim about its own behaviour is
  not evidence.
* `CallRisk::per_call` is true, so "allow for this conversation" on `exec` does not cover a
  server — a grant for a tool whose reach is the workspace is not a grant for a program whose
  reach is the machine. **On the pi core that rule lives in the bridge**, because the gate there is
  the app answering pi's dialog rather than the engine's `ApprovalGate`: `permit_covers` refuses any
  name `mcp::server_of` recognises. It used to check only `escapes.is_empty()`, so a yes given once
  for `mcp__fake__write_note` silenced the next call — one click becoming standing access to
  whatever that server can do. The first full pi-core pass is what caught it: the `mcp` group's
  second-call claim passed on the Rust core and failed on pi. The name test goes through the
  engine's parser rather than a `starts_with("mcp__")` in the bridge, because a second naming rule
  would be a second rule that can drift.
* A path in the arguments that lands outside the workspace is an **escape**, named on the card in
  every mode but Auto. A server cannot grant itself room cwoffee declined to give it.
* A dead server is a sentence, not a hang: `timeout_secs` from `[mcp]`, the same clock the app
  already uses for approvals. There is no third timeout, because a magic timeout is a hang with a
  decimal point in it.

## Two cores, one hub, two roads

`Hub` owns the children: an MCP server is a process whose pipes carry JSON-RPC frames, and the
hub is what keeps them readable across calls.

| | the Rust core | the pi core |
|---|---|---|
| where the hub lives | the app's process | the app's process |
| how a call reaches it | `Route::Hub` — the same `Hub::call` in process | `Route::Socket` — a unix socket in the app's state directory |
| who registers the tools | `mcp::install_into` | `mcp::install_socket_into`, from the catalog the hub hands out |
| what the withheld list is | the registry's hidden set | the same registry, filled from the hub's catalog |

The pi core cannot own a hub: its tool host is `cwtool`, one process per tool call, and a server
spawned inside it would be a new server on every call — a stateful one would answer correctly
exactly once. So the app starts the servers there too, and the tool process asks.

`crates/engine/src/mcp/socket.rs` is the whole road. It answers four things — `catalog`, `block`,
`status`, `call` — and refuses anything else by name. It cannot start a turn, change a model,
read a conversation, or approve: the approval gate is upstream of it, in the same place it sits
on the Rust core, and a process that can reach the socket can reach nothing else through it. The
socket is in a directory the app made for itself, is not a network port, and carries no token
because it carries no authority.

Two properties the tests pin down:

* The serving task runs on the **app's own runtime**. The servers are tokio children with tokio
  pipes, and driving the hub from a second runtime leaves those pipes on a reactor nobody polls:
  a live server would answer nothing and read as a dead one at the timeout.
* `McpTool::run` is one function with two arms. Risk, escapes, `per_call`, the truncation limit
  and the wording of a failure are all decided before the route is consulted, so a call over the
  socket is the same call, and a dead server is described in one sentence on either core.

When the socket could not be opened the app says so at boot and hands out no path; the tools then
answer "the app's MCP hub is not reachable at …, nothing was called" rather than guessing at a
server. The servers page prints the route (`this process`, the socket
path, or a hub no tool process can reach) so a row cannot read as *mounted* while no call can get
there.

## What the servers page answers

The door is in the sidebar, above Settings, and it opens a page rather than a popup: the thing a
person is trying to read while they change one of these is what else is already running, and a modal
hides exactly that.

**Running now** — one line per server the *hub* started, plus what it offered: live with its tool
names spelled out, dead with the reason, or `off in config`. The names and not a count, because the
catalog the model sees is budgeted (`CATALOG_BYTE_BUDGET`) and can leave names out; "does it have a
write tool?" is only answerable by naming them. Beside it, how many names are withheld from the model
and the two wire numbers.

**In the config file** — one row per server the file lists, with what the *hub* says about it right
now (`running`, `dead`, `off`, `not started`), the command and arguments, and the **names** of its
environment keys, never their values. A master switch (`enabled` in `[mcp]`), a per-server switch, a
two-press Remove, and **Re-read the file** for a server typed in an editor instead of into the form. An
Edit prefills the environment as bare names, because the values are not this page's to show.

**Add or change** — name, command, arguments one per line, environment one per line, and whether to
start it now. Nothing is downloaded, installed or contacted: the form names a program that is already
on this machine, and its help says so.

**The search box** over both lists looks through server names, commands and *tool* names at once. The
question that sends a person there is usually "what did the thing called jira give me?", which is a
tool-name question, and a list filtered to nothing has to say *filtered* — a page that simply shows no
rows tells the reader they have no servers.

Six rules the writes obey:

* **The file is edited by line surgery, not by re-serializing TOML.** A hand-written file with
  comments, a table nobody here knows about, and an array wrapped across lines has to come back with
  all of it. A serializer would quietly rewrite the whole thing, and the person would lose
  annotations they never typed in this app.
* **A write says two things or lies.** The sentence is built by the engine (`add_server_with` and its
  neighbours) and then extended by the app with what the *process* did — `written to
  [mcp.servers.jira]; jira-mcp: live (3 tool(s))`. Writing the file and starting the program are one
  act here; the file alone is not evidence a server works, which is why the cargo test that asserted
  only the file no longer describes the feature.
* **`CWOFFEE_MCP_SERVERS` refuses the write.** That variable replaces the file's `[mcp.servers]` list
  wholesale, so a form that wrote the file would report a success the next start ignores. The payload
  says `env_lists: true, writable: false` and the page says why instead of offering controls that
  cannot work.
* **A removed server stops being callable in the session that removed it.** The hub forgets the
  program, and the registry forgets the handles — `Registry::forget_stale_mcp`, called from the same
  refresh that registers new ones, holding the set of names the hub still offers. Otherwise the model
  can spend the rest of the conversation naming `mcp__jira__create`, be shown an approval card for a
  program that is no longer configured, and be refused only when the hub answers. The `mcp__` prefix is
  the filter: a first-party tool is not the hub's to offer, so this function cannot take one away.
* **An edit that never saw a token keeps it.** The page lists an environment's *names* — a value on a
  settings row is a secret in the next screenshot — so the form prefills bare names, and a bare `KEY`
  line means "keep what the file holds". Only `KEY=value` writes a value, and `KEY=` is a deliberate
  empty. `McpServerCfg::env_keep_lines` resolves that against the row already in the file; a name the
  server does not have is refused rather than created empty. Without the rule, opening a server to fix
  its command would blank every token it had and report a successful edit that broke the program.
* **A phone tab is told it is a phone tab.** The gateway reports the same status with `writable:
  false`: the programs run on the machine that started them. Desktop and gateway build that payload
  from one function (`mcp::status_payload`), so the two cannot drift into disagreeing about what
  "running" means.

A removed server loses both tables — `[mcp.servers.jira]` and `[mcp.servers.jira.env]` — and leaves no
orphan. A switched-off server keeps its entry and its arguments: it stops the process and writes
`enabled = false`, which is the honest way to park one. And the handles the model could call go with
it: the registry drops the third-party names the hub no longer offers, so a conversation cannot spend
its remaining turns naming a tool of a program that was removed mid-session.

## Checks

* `cargo test -p cwoffee-engine --lib mcp::` — the framing against `scripts/fake-mcp-server.mjs`,
  the catalog ceiling, the hidden-until-revealed economy, the per-call rule, the timeout, and the
  socket: one call two roads answering identically, a dead server described identically, a
  refusal of anything that is not a tool call, a host that is gone answering in words and
  quickly, and a catalog reached over the socket declaring nothing until named. Plus the write
  path's other half: `a_removed_servers_tools_stop_being_callable_in_the_running_session` registers
  three tools hidden, takes one server away, and requires the handle, the withheld name and the
  declaration all to be gone — while `write_file`, which the hub never offered, survives the sweep.
* `scripts/selftest.sh mcp` — on **both** cores: two servers start, nothing is declared before
  `explore` names it, the card leads with `MCP server fake`, the file the server wrote is on disk
  with its words in it, a session grant does not cover the next call, Deny moves nothing, and a
  call into the server that had exited comes back naming it. The scenario's `require` strings are
  checked against the bytes the app sent, so the catalog block reaching the model is graded there
  too.
* `scripts/selftest.sh mcp-ui` — the page as an actor, on **both** cores and in **two arms**: the
  board's run, whose servers come from `CWOFFEE_MCP_SERVERS` and which must therefore find the page
  read-only and told why, and the **mcp write** arm, a second window whose servers come from a scratch
  `config.toml` and where the page may write. The door sits above Settings and opens the page; an
  added server appears as a row, in the host's own `config.toml` (read back through the host's own
  path, not one the page guessed), and as `live (N tool(s))` in the hub; on the Rust core its tools
  join the withheld list and *not* the declared one, which is the whole economy of the feature
  re-measured mid-session; the switch reaches the file and the process; an edit of the arguments moves
  the running set and drops the tool that the new argument count argued out of existence, while the
  environment value the form never showed survives the save; Remove needs two presses and leaves
  neither table in the file and no server in the hub; and the file ends the run the size it started.
  The search is graded as a lookup by tool name, and a filtered-to-empty list has to say it is
  filtered rather than read as "you have no servers".
* `scripts/gateway-prompt-shape.mjs --core pi` — reads the request bodies and asserts the two
  wire numbers: the `<mcp>` block is within its ceiling and says what it left out, and no
  `mcp__` tool declaration is in the request before anything asked. It configures 42 tools across
  two servers so an uncapped catalog would fail it.
