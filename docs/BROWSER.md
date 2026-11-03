# The browser: so a model can run the thing it just wrote

`exec` can start a dev server. `read_file` can read the source. Neither can say whether the page
renders, whether the button works, or whether the console is on fire — and that gap is where
"I tested it" has always meant "it compiled". `browse` opens a URL in a real browser, walks a
short scripted sequence through it, and reports the page's own account of itself: what it
rendered, what its scripts logged, which requests failed, and a picture when the model can
actually look at one.

## Two browsers, and they are not the same tool

There are now two things that can open a page, and the difference between them is accountability,
note chrome:

| | one-shot `browse` | the **kept** browser |
|---|---|---|
| who holds it | the tool call itself | the app process (never `cwtool`) |
| lifetime | one walk: open, act, report, close | until nobody touches it for `[browser] idle_secs` (5 min by default) |
| can the person see it | no | yes — the window's browser panel, on the right of the transcript |
| approval | one card per walk, naming one origin | a card **every call** (`per_call`), even after a Session grant |
| how the model asks | `browse` with `session` left out — **unless this start has a page of its own, see below** | `browse` with `"session": true` |
| what the call may act on | the url it names | whatever page is already open, including one the *person* opened |

**When the app is showing pages in its own window, that browser is the browser.** A start with a page
in the window has a native driver mounted in its hub, and `browse` routes to it: the call gets the
page the person can see, and cwoffee never opens a second browser beside it. That is not a
preference — a Playwright window on the desktop, running a page the person cannot look at while the
one they *are* looking at sits idle, is the failure the whole feature exists to avoid, and it is what
happened when routing was left to the model remembering `session`. Three ways out, all spelled:

* `"session": true` says the same thing by name, and is still what a start without a page of its own
  needs.
* `"session": false` is the deliberate exception: a walk that must not touch the person's page. It is
  not quietly redirected, and it paints no window either — behind the sidebar's back is exactly what
  it is asking for.
* `[browser] keep = false` means the person refused a standing browser, so the one-shot walk is what
  they want and the routing stands down.

Routing a call into the window's page also changes what an approval means. A Session grant was given
to a browser that opened and closed inside one call; it cannot cover clicking around inside a page a
person is watching, so `risk_for` forces `per_call` for a routed walk too — the same reason an MCP
call never inherits a remembered grant.

The one-shot path is otherwise unchanged: one walk is one approval card naming one origin, `steps` is
capped at 40, the walk has one deadline (`timeout_secs`, ceiling 120 s), and the driver keeps its own
clock so a hung page becomes a sentence instead of a stuck tool.

The kept browser used to be refused outright, and the reasons in this file's older draft were
right about the thing they were guarding: a window on somebody's desktop between turns, with
nobody accountable for what it is showing, is not a feature. What makes it ship now is that each
of those objections has an answer in the code rather than in a hope:

* **the window is the point.** The panel is a *window onto that browser*, opened by a person or by
  the model starting one, so "a window with nobody seeing it" is the case that no longer exists.
* **the accountability moved to the card.** A `session: true` call is asked for every time.
  "Yes to browse" was a decision about a throwaway browser that closed when the call ended; it is
  not a decision about clicking inside a page the person has open and may be logged into. The card
  says which browser it is: the throwaway one says it closes at the end of the call, the kept one
  says it stays open in the person's sidebar and may be theirs.
* **it does not linger.** A reaper task the app starts closes it after the configured span, the
  driver has a longer backstop of its own, closing the app closes it, and the panel shows the
  countdown while it runs. `keep = false` makes the whole feature absent, and says so.
* **the pi core's wall is crossed, not ignored.** `cwtool` is one process per call, so it cannot
  hold a browser; the app holds one and `cwtool` reaches it over a unix socket — the same shape
  [`MCP-SERVERS.md`](MCP-SERVERS.md) uses. The sequence a tool call runs is the same on both cores,
  because both go through one `Link` type; two sequences is how one core ends up clicking something
  the other never did.

## Headless or headed is the host's decision

The rule, in one place (`browser::decide`, `crates/engine/src/tools/browser.rs`):

| what is known | what runs | why the sentence says |
|---|---|---|
| `[browser] headed = true` in the config | a real window | "you set [browser] headed = true" |
| `[browser] headed = false` | headless | "you set [browser] headed = false" |
| this start has a page in its own window | headless | "cwoffee's own window is the browser in this start…" |
| model takes image input | a real window | "this model takes image input" |
| model takes text only | headless | "this model takes text only" |
| the endpoint never said | headless | "this endpoint never said whether the model sees images, so nothing was painted on your desktop" |

Two things are load-bearing here.

**The model cannot ask.** There is no `headed` field in the schema, and a unit test asserts that
neither `headed` nor `headless` appears in it. A tool call is the one place a model gets to act,
and "put a window on a human's desktop" is not something an agent should be able to request from
inside the prompt. Everything else about the walk — URL, steps, whether a picture is taken — the
model does choose; this one is withheld on purpose.

**`Unknown` is not `Yes`.** An endpoint that has not answered `/models` has not earned the right to
draw a window on someone's screen, and the answer says it was silence rather than letting the run
look like a deliberate headless one. The modality comes from the config file (`[models.<id>]
supports_vision`), which is the same row the settings screen writes from a probe: the file that
records what the model can see is the file that decides whether to show it something.

A screenshot the model cannot see is prompt cost with no reader, so for a text-only model the
picture is **saved and named but not attached** — `.cws/browser/<stamp>.png`, filed as
`FileKind::Scratch`, so it is never presented as a deliverable the person asked for. "I looked at
the page" stays true; the model just reads the page's words instead of its pixels. For a model
that can look, the picture is attached to the tool result (`ToolResult::images`) and reaches the
endpoint as an image inside the tool result.

## Pixels, and where each API wants them

`ToolResult.images` is model-facing (unlike `files`, which is UI-facing, and `changes`, which is
both). The two provider shapes disagree about where a picture inside a tool result goes, and both
are graded so nobody "unifies" them:

* **Anthropic**: image blocks *inside* the `tool_result` block, text after them.
* **OpenAI-shaped**: the `tool` role takes a string, so the picture follows as its own `user`
  message, captioned `[image from the tool result quoted above]`. A `tool_call` left unanswered is
  a 400 from the endpoint, and a picture with no caption is a picture from nowhere.

A tool result with no images produces exactly the request it produced before this tool existed: no
extra message, no extra bytes.

## What a page may reach

`risk_for` answers per call, because a browser is not one thing:

* `http(s)://` — `Risk::Sensitive`, with `per_call: false`. Not `Harmless`, even though `web_fetch`
  is: a fetch *reads* a page, a browser *runs* it — somebody else's scripts, in a real renderer,
  over a network stack that is not the sandboxed one `exec` commands go through. Ask mode therefore
  sees one card naming the origin; Session and Auto do not click a card forty times for a model
  reloading the page it just wrote. The proxy gap is stated in the tool's own description rather
  than hidden: a page fetches for itself, so its requests are not routed through the sandbox's
  package-manager allowlist proxy. `[browser] enabled = false` takes the door away entirely.
* `file://` **inside the workspace** — `Risk::Workspace`. It is the same read the file tools
  already have.
* `file://` **outside it** — `Risk::Sensitive` with the folder named as an escape, in every mode
  but Auto. A page can read its own folder's siblings, so this is a read of the machine, and the
  card has to name the folder or nobody can decide on it.

The `file://` path is resolved to an **absolute** path before the decision, and the browser is
pointed at the resolved path rather than at the caller's spelling. An earlier shape of this check
stripped the leading slashes and joined what was left onto the workspace, which made
`file:///etc/hostname` look like a page inside the folder — and then handed the browser the
original URL, so the browser really did read `/etc/hostname`. A confinement check built out of
relative strings cannot catch that; the test says so by name.

URLs with a host in them (`file://othermachine/x`) are refused rather than guessed at: the answer
would be "some other machine's filesystem", which this tool cannot judge. Downloads are declined
and reported: a browser tool that quietly writes files outside the workspace would be a hole in
the folder rule.

## It is mounted only where a driver exists

`browse`'s schema is ~1.1 KB of every prompt, on both cores. A machine with no Playwright must not
pay that to be told "not installed" on the one turn the model tries it, so the tool is registered
only when a driver is found (`Registry::with_v1_tools_browser`). The lookup (`Config::browser_cfg`)
takes `[browser] driver`, then `CWOFFEE_BROWSER_DRIVER`, then the two places cwoffee actually
ships it: next to the program, and next to its source tree.

Nothing in cwoffee downloads a browser. A ~150 MB binary arriving on this machine is a decision a
person makes by running a command, so the answer when nothing is installed carries both commands:

```
npm --prefix browser install        # the driver (browser/driver.mjs, playwright-core)
npx playwright install chromium     # the browser build, in ~/.cache/ms-playwright
```

The driver prefers `chrome-headless-shell` for a headless walk and requires full Chromium for a
headed one — a headed run on a headless-shell build is a window that cannot be painted, and it is
reported as that rather than as a page failure.

`browse` is appended after `explore` in the tool list, so mounting it does not reshuffle the tools
before it and throw away every running session's prefix cache for a feature the conversation may
never use.

## Both cores, one source of truth

| | the Rust core | the pi core |
|---|---|---|
| who runs the driver | the app's own tool call | `cwtool`, one process per call |
| where the config comes from | `Config::load` in the host | `Config::load` in `cwtool` |
| how the model's eyes are known | `config::vision_in(cfg_file, model)` per turn | the same file, with `CWOFFEE_MODEL` naming the model the call belongs to |
| how a driver is found | `Config::browser_cfg()` | the same function, or `BrowserCfg::from_env()` if the file cannot be read |
| who holds a **kept** browser | the app, reached in-process | the app, reached over `cwoffee-browser.sock` (`CWOFFEE_BROWSER_SESSION_SOCK`) |

The bridge hands the tool process the model's *name* rather than a verdict about its eyes, so
there is exactly one function that decides headless versus headed. A duplicated rule in
`crates/bridge` or in `pi-tools/cwoffee-tools.ts` would disagree with the engine the first time a
model changed mid-session, and the disagreement would be visible as a window appearing on a
desktop for a model that cannot see it.

The host also publishes the answer it is running with: `HostState::browser_vision` is `yes`, `no`
or `unknown`, refreshed at the start of every turn from the config row of the model that is
answering. Frozen at spawn it was wrong in two ordinary cases — a probe recording modalities after
boot, and a model chosen mid-conversation — and in both the settings screen and the tool would have
disagreed about a window. The selftest's `browse` group caught exactly that disagreement, and
`the_browsers_mode_follows_the_config_row_and_not_the_boot_snapshot` now grades it at the host.

The settings line comes from the same policy: `browser_status` (a Tauri command and a gateway
**watch** verb) returns `mounted`, `driver`, `node`, `headed`, `why`, `screenshot_default`,
`vision` and `says`, where `says` is `browser::status_line` — the tool's own words, not a
paraphrase written in the UI. It takes the model as an argument because the answer is about the
model that is answering.

## What the model is handed

Order matters: the verdict line first, so a result truncated by the output ceiling still says what
happened. Then the mode and why, then each step's outcome, then **the page's own code** (uncaught
errors, console errors and warnings), then failed requests, then declined downloads, then the
controls on the page, then the screenshot's fate, then the page's text, then timing. The page's
own complaints come before its prose because the reason this tool exists is that a page can look
fine in its text and be broken in its scripts.

## The kept browser, and the panel that shows it

`crates/engine/src/browser_hub/` holds it. The rules, and the reason each one exists:

**The app owns it; a tool process borrows it.** `Hub::mount` spawns nothing — the browser starts on
the first `open`, so a running app with no browser costs nothing. On the pi core the bridge passes
`CWOFFEE_BROWSER_SESSION_SOCK` to every `cwtool`, and `BrowserRoute::over_socket` reaches the app's
hub; the Rust core uses `BrowserRoute::in_process`. The route is built once per start, and
`refresh_blocking()` is called once at tool startup — not inside `peek()`, because `peek()` is what
a **risk** check reads, and a risk check that awaits a lock is a risk check that can hang or answer
from a stale copy.

**Reads that cannot await get a mirror.** `Snapshot` lives in an `Arc<RwLock<…>>` that is written
by the hub and read synchronously, which is how `risk_for` can say "this is the browser somebody
is looking at" without a lock it must not take.

**The clock is derived, never copied.** `Snapshot::idle_deadline` is a moment, and the panel shows
`idle_left_now()`. A panel that printed a countdown read at the last action is a panel that is wrong
about the one thing the person is asking. `idle_words` renders the number the person configured —
`45` reads "45 seconds", `300` reads "5 minutes" — in the tool's report and in the panel, so a
browser that closed after 45 seconds is never described as having closed after a minute.

**The panel is not a browser.** It draws *pictures of the page the tool is acting on*. Embedding
a second webview in that column would look identical and be a different browser: different profile,
different cookies, clicks the model never made — an agreement between two screens that no check can
catch and a person cannot see through. Frames are pulled, not pushed over the wire: the panel asks
"anything newer than picture N?" and usually gets a number and no bytes back.

**While the column is open, the browser sends its own pictures.** A page that repaints with nothing
acting on it — a video, a spinner, a clock, a page still loading — was invisible in the column: the
panel drew a screenshot of the page as of the last act, and then sat there. So while the panel is
open it sends `browser_watch{on:true}`, which becomes `watch` to the driver, which starts Chromium's
CDP screencast (`Page.startScreencast`) and pushes a JPEG each time the page paints. Two things about
that are load-bearing. The panel's *live* mark reads `live` from the driver's answer, not from its own
ask: a screencast Chromium refuses, or a headless shell that does not implement it, must not be shown
as a running view. And `watch` is not activity — it never touches `last_activity_at`, because watching
a browser is not using one, and the five-minute promise the panel counts down is a promise about
nobody *using* it. Hiding the column, hiding the window, or destroying the component sends `off`,
which stops the browser encoding frames for nobody; a stream left running on a hidden panel is work
with no viewer. Frames arrive as `frame` events, are stored under a **picture** id of their own
(`Frame.pic`, `Snapshot.frame_pic`), and the poller compares that id — so a volunteered frame reaches
the screen without any act having bumped the page sequence. What arrives is gated twice: the driver
pacing (`CWOFFEE_BROWSER_VIEW_MIN_MS`, default 85ms) and a quality sized to the panel's width
(`CWOFFEE_BROWSER_VIEW_QUALITY`, default 55).

**The ask carries the box, so the picture is not the page.** A page is 1280-ish wide and the column
that shows it is often 400px; a screencast sized to the page sends about a hundred kilobytes of JPEG
per picture to a box that draws a third of it. So `watch` carries `max_width` — the stage's own width
in CSS pixels — and the driver sizes the stream to it, restarting the cast when the box changes
because the watcher re-states the ask whenever it measures the box (a window resize, full window).
Measured on this board by the probe below: **2.3 KB a picture at 398px against 11.7 KB at 1258px**.
What does not change is the page: `state` still reports the page's own viewport, clicks are still page
points, and a press on a 398px picture of a 1280px page lands on the page pixel it stands for. The
picture got smaller; the page did not.

**A picture is owed to the viewer by the page, not by a timer.** Two sequences look alike and mean
different things: `snap.seq` counts *page state* (every act and every page event bumps it), while
what the panel holds is the id of a *picture*. A reload bumps the state and takes no picture of its
own, so answering "you already hold the newest" to a caller comparing its picture against the state
leaves the pre-reload page on the screen — which is exactly what happened, and the panel looked alive
while showing a page that no longer existed. `browser_hub::needs_pic` answers that question once, over
both ids, and the desktop command and the gateway arm both call it rather than each holding its own
comparison; `frame_pictures_state` is the shared predicate, since "does the newest picture picture the
newest state" is one fact with two callers. The client's variable is named `browserFrameSeq` for the
same reason: a payload about the page is not a payload about the picture.

**Full window asks the page to fill the box.** The column runs the height of the window — a fixed
header and footer, a page area that grows, and a log that shrinks before the page does — and the
control at its right edge hands the whole window to the page. That is not only a CSS question: a
1280×800 page shown in a 1260×521 box is a page with slack above and below it, and the slack is the
complaint the person has. So the wide mode measures the page area and sends `browser_viewport` with
that box; the browser reports the viewport it *has*, and the panel scales its presses by that number
rather than the one it asked for, because the ask is a request and the report is a fact.

**A refusal the person caused stays up until they do something else.** The panel is polled, and a
poll that clears the error line wipes a refusal about a second after it appeared — the sentence
explaining why the address box said no would be gone before anyone read it. So the panel keeps the
kind of failure it is showing: a failed *state read* is transient and the next good read clears it, a
*refusal* is kept until the next action replaces it.

**The panel's file rule is narrower than the model's.** `browser_open` accepts `http://` and
`https://` only. A page box in a panel has no per-call approval card behind it, so there is nothing
to answer "may this read `/etc/hostname`?" with; the model's `file://` rule stays confined by
`file_open_rule`, which resolves the path to an absolute path before deciding. The refusal says what
the panel does open, and the self-test grades that a refused address does not replace the page that
was open.

**A switched-off kept browser is a person's decision, not a broken feature.** `[browser] keep =
false` gives `SessionCfg::off_reason`, and the panel says "kept browsers are switched off ([browser]
keep = false in cwoffee's config)" rather than offering a box that can only fail. A *missing*
driver gets the other sentence, with both install commands in it. `panel_says` picks between them;
`--no-browser` on the board runs the switched-off arm so that sentence is graded and not merely
written.

**The column is shaped like a browser, and its controls are icons that carry their names.** An
address field, Back, Forward, Reload, a page area, and a footer with the one destructive thing in it —
because a person who has used a browser knows what each part is for, and a panel that invents its own
vocabulary has to be learned. The controls are glyphs, and each one carries its name twice over: an
accessible label a keyboard and a screen reader read, and a tooltip that says what the control *does*
("re-reads the page that is open; it does not go back") rather than what it is called. A column 33vw
wide has no room for "Back to the previous page" as a button, and a bare glyph is only safe when the
words travel with it.

**Nothing open is drawn as nothing open, not as a blank box.** When no browser is alive, the page area
shows the outline of a page — a chrome row with two dots and an address pill, then three lines of text
at different widths, drawn with the same hairlines a page is drawn with — over the words "No browser is
open" and the engine's own reason. A plain white rectangle is ambiguous: it can be read as a page that
failed to load, which is a different fact, and one nobody should have to diagnose from a shape. The
stage sits on `--color-raise`, the one surface token that is a step off the paper in both palettes, so
the page's own pixels read as a rectangle *on* the column instead of a hole in it. A watching column
says so where it would otherwise offer a control: the address field carries "this window only watches
the browser", and the footer says a page "opens when the assistant opens a page — this window cannot
start one". A disabled input with a placeholder inviting typing is a claim about a phone tab that the
phone tab does not have.

**Pointing at the picture is pointing at the page.** The frame is drawn as something to aim at: a click
in it sends `{do: "click_xy", x, y}` in *page* pixels, and a wheel over it sends `{do: "scroll", px, x,
y}` at the point under the pointer. The scaling is the panel's job because the panel is the only party
holding both boxes — it knows the picture's rect on screen, and the browser reports the viewport in
`state`, which is the size of the *page*, not of the picture (a live stream is sized to the box, a
still to the page). A page point outside that viewport is refused
by the driver rather than clicked at, and the panel marks the spot it pressed with a ripple, because a
click that landed somewhere other than where it was aimed is otherwise invisible until the page happens
to move. Keys go through their own row (`{do: "type_keys"}`) into whatever the page already has focus,
which is what a person gets after clicking a field in the picture: `fill` needs a selector, and somebody
aiming with a mouse has no selector to name.

**The page's own complaints are shown, not swallowed.** The column has three views — the page, what the
page reported, and what this browser is. The middle one is the reason: `pageerror`, failed requests and
refused downloads arrive from the browser whether anybody asks for them, and a column that shows a
screenshot while hiding "the page threw a TypeError twice" is showing half the picture.

**The idle clock is drawn as a ring, and the words are still there.** A ring that empties answers "how
much of the promise is left" without being read; the sentence ("closes in 5m00s — 300 seconds with
nobody touching it") stays in the DOM for a screen reader, for a check, and for anybody who wants the
unit the config actually uses.

**Closing is two presses.** One click closing a browser the model may be mid-walk in is a way to
break somebody else's work by accident; the first press arms, the second acts, and the picture goes
with the browser — a frame left on screen above a line saying the browser closed is a page that
still exists.

**It tells the model what is open.** When a page is open, the live env block says where it is, who
opened it, who touched it last, that `browse` with `"session": true` acts on that page, and that it
closes itself after the configured span. When nothing is open it says nothing about a browser, so
the block does not describe a browser that does not exist. A one-shot walk that runs while somebody
else's page is open gets a note to that effect, because "the model opened a second browser" and
"the model used the page I was looking at" look the same in a transcript.

**With nothing open, the column is a starting point.** The page area takes a page's own surface —
`--color-page`, white in the light palette, near-black in the dark one — rather than the desk colour,
and in the middle of it: the browser's mark, `Start browsing`, and `...or ask cwoffee to browse for
you.` It used to read "No browser is open" over the outline of a page, with the same sentence again
underneath. That is a fact about what is *missing*, drawn where an invitation should be. The shape is
graded in `browser-panel`, including that the second sentence is gone. A start that mounted no browser
at all says so instead — an invitation the column would refuse is a lie.

**The door is on the card it opens, and the two columns end on one line.** The browser button lives in
a row at the top of the transcript card (`data-card-tools`), not in the titlebar: it belongs to the
conversation, and the titlebar is the window's own controls. The row is a row rather than a button
floating in the corner, because the transcript scrolls under anything absolutely positioned inside the
card. The column's margins are the card's margins (10px top, 14px bottom), so the browser and the
transcript end on the same two lines instead of the column running a few pixels out of true. Both are
graded: `browser-panel` compares the column's top and bottom against `.panecard`, and
`browser-live-probe` does the same against the real window.

**Every event lands a record, and the record says who said it.** The two drivers speak two
vocabularies — the page in the window sends `diag`, the child-process browser sends flat `console`,
`pageerror`, `requestfailed`. Both fill the panel's bags *and* push a `Diag`, because the drawer and
the model's `page_log` read records: a person seeing an error while the model insists the page said
nothing is two readers of one fact disagreeing. Ids come from the app (`diag_seq`), never from the
page — a page counts from one per document, and an id is what a drawer keys rows by and what a reader
means by "newer than this". `source` is empty when the page spoke about itself and `hub` when only the
app could know (a document arriving, an address moving inside one page): "the page logged nothing" is
not the same answer as "nothing happened", and `note_move` writes a `navigation` record for every move
the hub hears about, asked for or not.

**A step that leaves the page waits for the page it is going to.** A click on an `<a href>` or a submit
is answered by WebKit before the navigation commits, and the old document stays scriptable for a
moment — long enough to answer `location.href` correctly about a page that no longer exists. The bridge
marks such a step `may_navigate`; the driver then polls the document id for `NAV_WAIT` (1.6s) before
answering, and the reply carries `navigated`, `url_from` and `url_after`. The tool prints which page
the words above came from, so "the click worked and the address changed" and "the click did nothing"
stop being the same sentence. A `download` link is the exception that keeps the rule honest: it names
a destination and the document stays exactly where it is, because the click that offers a file is not
a click that goes anywhere (see *A file the page offers*, below).

**A step that is still running when its document goes away gets answered, not waited out.** The app hands
the step to the page and then polls for the answer, and the page answers in one of two forms: `a|` is
the answer, `b|` is "not answered yet — and here is the id of the document that is up now". The second
form is the point. A Back, a submit, a link click replaces the document that was going to answer, and the
table its answer was going into goes with it; polling that table until the walk's budget runs out is how
a step that *worked* came to report "the page did not answer within 20s". So the poll compares boot ids,
and when they differ the step is closed with the address that is up. For the verbs whose whole job was to
move — `back`, `forward`, `reload`, `navigate` — that is the outcome, so the reply says `ok`, names the
new address and says plainly that the page's own answer was lost; for any other step it says what moved
and refuses to guess whether the step landed. `asked_to_move` is the seam; the two halves of the protocol
are joined by three names (`__cwR`, `__cwDone`, `__cw.boot`) and `the_page_and_the_app_agree_on_where_an_answer_lives`
fails if either side renames one.

**A wait says how long it waited, and a `wait_for` looks again.** Every wait in the system is measured by
the driver that performed it and printed as milliseconds, because the alternative — echoing what was
asked for — makes a step that did nothing indistinguishable from one that waited. A `wait_for` that
cannot find its thing used to answer in three milliseconds, which told the model the page had failed at
its own job when the page had simply not finished doing it; all three drivers now look again for the time
the step was given and, when they give up, say how long they looked. A `wait` the step has no room for
says so (`"this step only had 600ms to answer in, so it waited 600ms of the 4000ms asked for"`) rather
than quietly truncating. Graded against a real page by `scripts/browser-bridge-probe.mjs` (the page in
the window) and `scripts/browser-session-probe.mjs` (the kept browser and the one-shot walk), which is
the only way to catch the three of them disagreeing: a `wait` that is a no-op in one driver and four
seconds in another is invisible from either side alone.

**One step list, in four files, checked against all four.** The schema a model reads and the three
browsers that answer it used to be separate lists: `a11y`, `health`, `back`, `forward`, `reload` and
`stop` were advertised everywhere and answered by two of the three browsers, so a walk got `I do not
know the step "text"` and a model concluded the page had no words. The list is now one constant
(`VERBS`) with a test that reads the schema *and* the three driver sources in this tree and fails if a
verb is missing from any of them, and the walk answers the whole list — including a history of its own
(`back` after a `navigate` goes somewhere) and a `health` counted from the one document it had.

**When a browser has never heard of a documented step, the sentence says which half is old.** The
browsers are Node files sitting *beside the app binary*, and an install that moved the binary and left
them behind is invisible everywhere else: the tool advertises steps, the browser denies them, and the
only symptom is a model giving up on a page. `browse` now appends the diagnosis to the report, and at
boot the app prints which copy of itself is running, when it was last replaced, and the age of the
driver files next to it — with a warning when those are older than the binary. The warning only fires
for an installed copy (`CWOFFEE_INSTALLED=1`, written by `scripts/install-desktop.sh` into the
launcher it generates): in a source tree the drivers are sources and older by nature, and a warning
that prints on every dev run teaches a person to ignore warnings.

**A name that matches several things says what they are.** "2 elements match `#p-search input`; name it
more narrowly" costs a model a round trip, because it cannot write a narrower selector than the page
can show it. Both drivers now list the candidates — tag, id, first classes, placeholder, the words
inside — and say nothing was touched.

**The box follows the folder the conversation is in, not the folder the app started in.** A local
page may be opened from the working folder — the same boundary the model's file tools use. Both
halves ask the engine which folder that is *now*: the address box (`live_workspace`), and the page
box's navigation guard, which holds a function call (`WsOf`) instead of a path copied at startup.
The guard had the copy, so after the person opened a conversation in another folder every page in
the folder they were standing in was refused as a link out of the workspace — a refusal naming a
folder they were not in. `a_local_page_is_confined_to_the_folder_the_conversation_is_in_now` flips
the folder between calls and grades both directions.

**Waiting is not touching.** A click that matches four elements is refused and the candidates are
named, because guessing harms the wrong one. A `wait_for` that matches four is answered by the first
that appears: "has it arrived?" is not a question about *which* one, and refusing there costs a round
trip to re-name a page the model has already read. The answer says how many matched, so "it appeared"
is never read as "there was only one". Both drivers do this, and both keep the strict rule for the
verbs that act.

**A step that runs out of time says what it had and what it touched.** The deadline is the walk's
(`timeout_secs`) minus the seconds already spent, and the report says both: the seconds it had left,
the seconds the walk was given, how far it got and which step was running, and the partial page
report — `health` first, because "the console is on fire" is the finding that survives a timeout. A
timed-out walk used to answer with a literal `{}` and nothing else, which reads as a broken tool
rather than a slow page. A **one-shot** walk cannot make that promise at all — its browser closes
with the walk, so there is no partial report to read — and its timeout sentence says so and points
at the kept browser, where the page and its records outlive the step that timed out.

**One clock per step.** The deadline the walk carries travels into the page box: `act` carries
`timeout_ms`, and the box's own wait for an answer from the document is clamped to it (floor 0.5s,
ceiling 25s). With a constant on each side, a 10-second walk had a step that could still be running
30 seconds later — the box said "the page did not answer within 30s" *after* the tool had already
said "the browser did not answer within 10s", and the pair reads as a broken box rather than a slow
page. Graded by `a_step_is_given_the_time_the_walk_has_left_and_never_more_than_it_can_answer_inside`.

### The verbs the kept browser answers

`browser/session.mjs` is one process speaking ndjson, and these are the verbs on the wire. The list is
written out because the two callers are different shapes — a model's `steps` array and a person's
clicks in a column — and an act vocabulary that only one of them can use is a feature with one user:

| verb | who reaches for it | what it is |
| --- | --- | --- |
| `open` | the model (a url) and the person (the address field) | start the page; the driver reports the status, the title and its viewport |
| `click`, `fill`, `press`, `scroll`, `wait`, `wait_for` | the model's walk, the column's controls | aimed by a target the page matches, refused when two things match |
| `eval`, `text`, `snapshot` | the model, and a check that needs the page's own answer | read the page rather than describe it |
| `a11y`, `health` | the model, on a page the app is keeping | the elements a person can reach, and one sentence about how the page is doing — both as **text**, because an answer in a shape nobody renders reports `ok` and then goes silent |
| `back`, `forward`, `reload`, `navigate` | the column, and the model's walk | history is history; reload is reload; navigate moves this same browser to another address |
| `click_xy` | the column, from a press on the picture | a real mouse click at page pixels |
| `type_keys` | the column's keys row | keystrokes into whatever the page already has focus |
| `frame` | whoever is watching | the picture, with the viewport it was taken at |
| `watch` / `unwatch` | the panel, only while it is open | the browser starts and stops sending its own pictures, sized to the panel's box (`max_width`); watching is **not** activity, so it never resets the idle clock |
| `viewport` | the panel, in full-window mode | reshape the page to the box it is shown in, so a tall window is not a letterbox |

`watch` and `viewport` are not in the socket door's whitelist of verbs a borrowed process may send:
the stream and the shape of the page belong to whoever is looking at it, and a tool process that
wanted either has `session: true` and a page to act on. A watching device tab gets frames; it cannot
ask for more of them.

A point outside the viewport is refused rather than clicked at, and the refusal names the viewport it
was measured against: a click that lands past the edge of the page is a click on nothing, and "nothing"
looks like success from here.

## The settings that control it

One `[browser]` table, both browsers:

| key | what it decides |
|---|---|
| `enabled` | the `browse` tool at all (`false` takes the door away, and the settings line says so) |
| `headed` | a real window or headless, for the one-shot walk — and for a kept browser if set. Set it and the sentence says "you set [browser] headed = …" |
| `driver`, `node` | where `driver.mjs` is, and which node runs it. The kept driver is found **beside** it as `session.mjs` |
| `keep` | whether a kept browser exists at all (`false` = the panel says switched off, `browse` refuses `session: true` with the same words) |
| `idle_secs` | how long a kept browser may sit untouched. Default 300. The driver's own backstop is this plus three minutes, so the hub's clock is the one that fires first and the one whose words you read |

### The page in the window (the default; `CWOFFEE_BROWSER_PAGE=0` asks for the picture column)

There are two shapes of the same session, and the board picks one per start:

* **the page in the window** — the kept page is a document *this app paints*, in the box the panel
  leaves empty. It is what a browser panel is usually expected to be, and it is the default.
* **the picture column** (`CWOFFEE_BROWSER_PAGE=0`) — a browser in a child process whose pictures are
  relayed. Everything above this heading describes that shape.

Both answer the same line protocol to `browser_hub`, so the idle clock, the panel's sentences, the
who-opened-it record and the pending-command map are one set of code. What is different is where the
pixels come from, and therefore where the honesty has to be earned.

**Placement is a measured fact, not a hope.** The panel measures the empty box
(`[data-browser-pagearea]`) while it is open, states it through `browser_view_rect`, and re-states it
whenever its own layout changes — opening a page moves the box, and the box of a hidden panel is a
box of none, which is what takes the page off the conversation. On the GTK side the box is asked for
in `set_size_request` and `set_bounds`, and what the widget was **actually given** is read back off
the child's allocation and reported to the hub as `placed`. The panel then compares the box it asked
for with the box the widget holds and, if they disagree, says so in the column
(`[data-browser-placedoff]`) instead of letting a page sit in the wrong rectangle. Three GTK facts
that this caught, each in `BUILDRUN.md`:

* a `WebKitWebView`'s minimum height is the page's natural height, so a 1180px-wide page asked for
  589px of height and GTK gave the *sibling* band all of it — the page widget is wrapped in a box
  that is pinned to `set_size_request(-1, 0)`;
* `Fixed::add` stacks above the window's first child, so the app's own view is raised after the page
  is created — otherwise the page paints over the transcript and the clicks meant for the app go to
  the page;
* a hidden widget keeps the allocation it had, so the truth reported is allocation **and**
  visibility, and a page that is not visible is reported as nothing.

The page keeps its share of the column: the page band may not be squeezed to its CSS minimum by a
tall log band, so the panel refuses to let it fall below 45% of its own height and the logs live in a
drawer that opens on top of the page. The engine numbers that matter are in the hub's snapshot —
`want_*` (the box asked for) and `placed_*` (the box held) — and the panel's disagreement banner is
graded by a check, not left as a hope.

What else the shape gives, and costs:

* `crates/tauri-app/src/page_bridge.js` runs inside the page — installed before the page's own
  scripts, on every navigation. It watches (console, page errors, failed `fetch`/XHR, and the fact
  that a **person** clicked or typed, which counts as activity to the idle clock) and it acts
  (`click`, `click_xy`, `fill`, `type_keys`, `press`, `scroll`, `wait`, `wait_for`, `eval`, `text`,
  `snapshot`, `a11y`, `health`, `navigate`, `back`/`forward`/`reload`) with the same target language and
  refusal wording as `browser/session.mjs`, so a model is told the same thing about a page either way. A
  step's result is **polled for** (`__cwR[id]`, and `'b|' + __cw.boot` while it is still outstanding)
  rather than trusted from an eval callback: wry drops the callback of an eval issued before a load
  finished, so a lost answer would be a hang instead of a timeout — and the boot id in the `b|` reply is
  what lets the app notice that the document the step was issued into is gone.
* `crates/tauri-app/src/page_view.rs` answers the **same line protocol** as `session.mjs`, so
  `browser_hub` cannot tell the two apart — the idle clock, the panel's sentences, the
  who-opened-it record and the pending-command map are one set of code. The engine's tests prove it
  with a driver that answers in-process (`a_page_in_the_window_answers_the_same_commands…`,
  `the_idle_clock_closes_a_page_in_the_window_and_says_the_number`).
* A page already on screen has no picture of itself: `frame` is answered with that reason and
  `live` stays false, which the panel must draw as "the page is the page" rather than as an error.
  A vision model driving this browser reads the page (text, snapshot, eval) instead of looking at it;
  the one-shot `browse` walk still produces real screenshots.
* The engine is the system's WebKit, not Chromium: no Playwright actionability waits, no network
  interception, no request mocking. That is the trade, stated rather than discovered later.
* The page gets **no** app commands: the capability is scoped to the window's own webview, so a
  website loaded in that rectangle cannot call `files_read`.
* The page is one WebView until it is closed: an address typed at a page that is already open is a
  `load_url`, not a new widget, so the back list, the scroll position and a half-filled form survive.
  An address that moves within one document (a `#fragment`, a history step) is reported to the hub as
  a `url` event and **not** as a new page — the panel's address follows while the page count stays, and
  `back`/`forward` do not wait for a document that is on its way out: `history.back()` returns before the
  address changes, the answer written into the old document dies with it, and the app closes the step as
  the move it was (`ok`, plus the address now up) rather than spending the walk's budget on it.
* The page's **own** viewport wins. The box asked for is a request: window managers, scrollbars and
  scale factors can make the document smaller than the rectangle. The page says its size on every
  paint and the snapshot carries that, which is also what the panel's agreement check reads.
* A record's id is the app's, never the page's. A document counts its own diagnostics from one again,
  so trusting its number gives two records the same id seconds apart — and that id is what the drawer
  keys its rows by and what a reader means by "newer than 12". The hub stamps its own sequence, and
  the driver's cursor into the page's buffer is reset when the boot token says a new document arrived.
* What the address box accepts: `example.com` becomes `https://example.com`, `https://…` is taken as
  typed, an absolute path or a path relative to the workspace becomes a `file://` **inside** the
  workspace, and anything else is refused with the rule in its words. A `file://` outside the folder
  is refused by name and does not replace the page that was open; `javascript:` and `data:` are
  refused because the page box has no per-call path approval.
* `./target/.../cwoffee-app --page-probe` measures this shape without a harness: it opens a page,
  asks for a rectangle, and insists the widget's **allocation** matches it — the widget tree's own
  numbers, with one pixel of window chrome of slack, because the page floats in an overlay that sits
  at -1,-1 to cover the frame — before it drives the page —
  then walks open / click / eval / state / frame-refused / diagnostics / a11y / a measured wait / a
  wait_for that gives up / a self-navigation that nobody asked for / a second address in the same
  document / back / reload / an offered file, and prints the fixed's real geometry when a check fails.
  The later group is what the sidebar's own bug reports were about, so the probe grades them: that a
  `wait` moves the page's clock and reports the milliseconds it spent; that a step which leaves the
  page is answered in the moment it happens rather than at the end of the budget; that opening the page
  again does not replay the records it already gave (the same page-side id, kind and text twice is what
  a reset cursor looks like); that an error the browser hides says so instead of arriving as
  `Script error.`; and that a `download` link puts a file in `.cwoffee/downloads`, says so in the log,
  and leaves the page where it was. 31 checks, and it needs the built app and a display.

Environment, mostly for harnesses: `CWOFFEE_BROWSER_DRIVER`, `CWOFFEE_NODE`,
`CWOFFEE_BROWSER_KEEP`, `CWOFFEE_BROWSER_IDLE_SECS`, `CWOFFEE_BROWSER_SESSION_DRIVER` (an
override for the kept driver's path; a named path that does not exist is reported as missing
rather than falling back to the derived one), `CWOFFEE_BROWSER_SESSION_SOCK` (set by the bridge for
`cwtool`), and on the driver side `CWOFFEE_BROWSER_SESSION_HEADLESS=1`,
`CWOFFEE_BROWSER_VIEWPORT`, `CWOFFEE_BROWSER_PATH`, `CWOFFEE_BROWSER_HEADLESS_ONLY`,
`CWOFFEE_BROWSER_DRIVER_IDLE_MS`, `CWOFFEE_PAGE_PROBE_HOLD_MS` (leave the probe's page on screen — in the shape of the panel's column —
so a person can look at it or grab the window; it holds the page, not the app, and changes no verdict),
and the two that size the live view: `CWOFFEE_BROWSER_VIEW_MIN_MS`
(how often the browser is allowed to send a picture of a page that is busy repainting, default 85)
and `CWOFFEE_BROWSER_VIEW_QUALITY` (the JPEG quality of those pictures, default 55 — sized for a
column, not for a monitor). Neither is a config key: they are the shape of the pixels, and a person
who wants fewer of them closes the panel.

The board runs kept browsers **headless** (`CWOFFEE_BROWSER_SESSION_HEADLESS=1`): the panel's
pictures come from the page either way, and a run that measures window geometry has no business
putting a second real window on the desktop and then explaining the numbers it got. Nothing about
the panel depends on that, and `browser_panel` reports the mode rather than assuming it.

### What the model is told about the page it is standing in front of

`browse` answers a walk with the page's own words, and a page that has been open for a while keeps
answering after the walk:

* **`page_log`** — the records the hub has been keeping since the page opened, whichever way the
  driver chose to say it: a `diag` record from the page in the window, or a flat `console` /
  `pageerror` / `requestfailed` event from the child-process browser, both land as one kind of record
  (see the rules below). Console (by level), the
  page's uncaught errors with where in the page they happened, requests that came back an error or
  never finished with method, url and status, resources that failed to load, navigations, and downloads
  it would not make. `since` returns only what happened after the last time the model looked, `kind`
  and `level` narrow, and the reply carries a `health` summary — counts per kind, and how many of them
  are things that went wrong — so a model can decide whether the records are worth reading before
  reading them. It is `Harmless` (a read of an in-memory log the person can see too), it does not touch
  the page, and **reading it does not keep the page alive**: a log is not activity.
* **A file the page offers.** WebKitGTK stopped telling its embedder about downloads — the
  `download-started` signal wry connects has not fired for an ordinary link since 2.40 — so in this
  window a `download` link used to be a plain navigation: the address moved to the file, the page the
  person was reading was gone, and nothing was ever saved. Now the bridge cancels the default action
  of that one click and leaves a `download` record naming the file and the address to fetch; the app
  fetches it into `<workspace>/.cwoffee/downloads` and answers in the same log — `the page offered
  probe.zip` followed by `saved 57 bytes to .cwoffee/downloads/probe.zip`, or the rule that stopped it.
  The naming and the fetching are one module (`browser_hub::downloads`) shared with wry's still-live
  handler, so both routes write to one folder with one rule: a basename that cannot climb out, a
  `file:` offer only from inside the workspace, and a size cap. A file the browser *could* display is
  still saved rather than painted into the page — the page's own `download` attribute decides, not
  whatever the renderer happens to handle.
* **An error the browser will not name says that, instead of arriving as a shrug.** A page opened from
  `file:` — or a script from another origin — has an opaque origin, and WebKit hands `window.onerror`
  the single word `Script error.` with no filename and no position. The bridge uses the thrown object
  when it can reach one; when it cannot, the record says what was withheld and where to go read it
  (*open the page over http to get the message*), because a bare "Script error." is indistinguishable
  from a page that failed quietly, and a model holding it has no way to know the browser is the one
  that kept the sentence.
* **Both sides of a refusal name what the other side can use.** An ambiguous target lists the
  candidates — tag, id, classes, placeholder, the words inside — because "name it more narrowly"
  alone sends a model back to guessing. A refused local page names the folder it is confined to,
  because a refusal that names a folder the person is not standing in is an argument, not an answer.
* `browse`'s `steps` carry `text`, `snapshot`, `a11y`, `health`, `back`, `forward`, `reload`, `stop` and
  `navigate` beside the click/fill/press/scroll/eval set; the last five and `a11y`/`health` need the kept
  browser — which, in a start with a page in its own window, is what every `browse` call gets. A step
  that waits reports the milliseconds it spent, measured by the driver, and `wait_for` looks for the
  thing until its budget runs out and then says how long it looked. A `fill` on a `<select>` chooses an
  option — by its value, then by the words a person reads — and a name that is on no list says what the
  list offers, because typing into a `<select>` answers with an engine's internals where a sentence
  about the page should be.
* A page in the window has no screenshot of itself, so that is not a fallback: a model driving it reads
  it. The one-shot walk over the relayed browser still produces real pictures for a model with eyes.
  When no pixels come back — refused, or empty, or bigger than a result here carries — the report says
  which of those it was and names the verbs that do work (`text`, `snapshot`, `a11y`, `eval`). A model
  handed silence asks again.

## Tests

* `cargo test -p cwoffee-engine --lib browser::` — the decision table (a person's override
  outranks the model's eyes, silence gets no window and says so, a seeing model defaults to a
  picture and a blind one defaults to none); the schema has no way to ask for a window; the step
  list checked against all four places it is written — the schema's enum, `driver.mjs`,
  `session.mjs` and `page_bridge.js`, so a verb advertised without a browser answering it fails
  here instead of in a person's session; a browser that answers `I do not know the step` named as
  the old half rather than left as a verdict on the page; `file://`
  inside the folder against outside it, with the escape named; `/etc/hostname` staying outside;
  the no-driver answer carrying both install commands; the picture being withheld from a model
  that cannot look and cut off over the byte ceiling; the tool being mounted only where a driver
  is, and after `explore`.
* `cargo test -p cwoffee-engine --lib provider::` — the picture's placement on both provider
  shapes, and a tool result with no picture staying one plain message.
* `cargo test -p cwoffee-gateway --lib` — `browser_status` is watch-role, admits when nothing is
  mounted (with the install commands), and reads the model's eyes out of the config file rather
  than being told them; and the two browser gates are graded by their **order**: a watching tab's
  `browser_viewport` is refused for the *role*, a drive tab's for the *missing browser*, and a
  watcher's `browser_watch` is never refused for watching. Read the other way round, a dropped role
  gate and an unrelated "no browser here" look identical.
* `cargo test -p cwoffee-engine --lib browser_hub::` — the kept browser over real pipes to a
  driver: open records who opened it and act records who touched it next; the page's own events
  reach the panel without anybody polling for them; a page that refuses is reported as the page's
  own refusal; a driver that dies answers with the reason instead of leaving the caller waiting;
  the idle clock closes a browser nobody is using and says the number; a frame is bytes for the
  watcher and the snapshot counts it; a risk check can see the open session without awaiting a
  lock; the words for an idle span are the unit the person configured; a hub with no driver says
  what is missing and how to put it back; a switched-off kept browser is reported as switched off
  rather than broken; the panel says *why* there is no browser (switch, missing file, or timeout)
  rather than only that there is none; the kept driver is found beside the one-shot one, and an
  environment that names it wins — without falling back when the named path is missing; a phone
  that cannot drive the browser is told so in the payload; an adopted config changes the clock
  without losing the page; a frame the browser **volunteers** reaches the viewer with an id of its
  own, and is known to be the browser's own rather than one somebody paid for, and stopping the
  watch stops it and closing clears it and a fresh session starts its own count; resizing the page
  to the panel reports the size that *actually happened*, and a box too small to read is refused
  without changing anything; a borrowed process over the socket door may open, act, pull and close,
  and is refused `watch` and `viewport` — and the still it pulls is marked as a picture paid for
  now, not one off the stream.
* `cargo test -p cwoffee-engine --lib tools::browser::` — a session call is asked for every time
  rather than granted once; the card says which browser and that it stays open; a session walk
  reports the page the person is looking at; a session walk with no kept browser says what to do
  instead; a throwaway walk says somebody else's browser is already open; a walk in a closed
  session names the state rather than inventing a page. And the routing itself: a start with a page
  of its own sends a `browse` that never mentioned `session` to that page (with no driver mounted,
  so a report that reads like a walk is proof of which browser it reached), the `"session": false`
  exception still goes looking for its own browser and never borrows the person's page, a routed
  walk is `per_call` rather than covered by a Session grant, and a window of its own outranks the
  model's eyes in `decide` — but not the person's written `headed`.
* `cargo test -p cwoffee-engine --lib browser_hub::downloads::` — the name a page may ask for cannot
  climb out of the folder (`%2e%2e%2f`, `..`, a bare `/`), a `file:` offer is only read from inside
  the workspace, an offered local file lands in `.cwoffee/downloads` and a second offer of the same
  name does not overwrite the first, and an address that is not a file says which rule refused it.

**One test needs a real browser and says so.** `cargo test -p cwoffee-engine --lib -- --ignored
--test-threads=1 browser` runs `the_real_browser_streams_frames_of_a_page_that_is_moving_on_its_own`:
the real `browser/session.mjs`, the real Chromium, and a page whose own timer repaints it. Every
other test in this file talks to `scripts/fake-browser-session.mjs`, which can answer *"here is a
frame"* but has no repaints to offer — and "the page is moving and the viewer sees it moving" is a
claim about repaints. It also checks the picture id keeps advancing without any command in front of
it, that the stream survives a viewport change, that the size the panel reads is the size the
browser has, and that no picture outlives the browser it was a picture of.
* `node scripts/browser-live-probe.mjs` — the panel's half, over the built UI bundle with a stubbed
  Tauri IPC whose browser verbs are answered by a **real** driver process. It grades what only a
  webview can grade: that a real page repainting on its own repaints the picture (two reads of the
  `src`, no act between them); that the picture in the panel is the browser's own pixels, drawn at the
  *box's* width rather than the page's; that a press on the picture of a known box arrives at the page
  point that box occupies (the box is the witness, by way of the driver's own `click_xy`); that the
  column spans the window and the picture area takes the slack under the footer; that the ask states
  the stage's width and the stream follows it in bytes as well as in size; that full window is the
  whole window and the page is then the shape of the box it is drawn in; that hiding the panel stops
  the stream; and that the idle clock is still on show while all of this is happening. Sixteen claims,
  and it needs a Chromium and `ui/dist`.
* `node scripts/browser-bridge-probe.mjs` — `crates/tauri-app/src/page_bridge.js` installed into a real
  Chromium page, polled with the same `a|`/`b|` script `page_view.rs` builds. The bridge is 900 lines of
  JavaScript that the Rust tests cannot load and the UI tests never reach, and every bug in the sidebar
  browser's step handling lived in the join between the two. It grades that a wait waits and says how
  long (the page's own clock, not the caller's word for it); that the page's timers and animation frames
  keep advancing while a step is in flight, which is the claim a "the renderer is starved" report can
  only be answered with; that `wait_for` looks again; that a promise handed back by `eval` is awaited and
  a rejection is a sentence; that a step which leaves the page is answered instead of waited out; that
  `a11y`, `health` and `text` come back as things to read; that `<select>`, Tab, and Enter in a form do
  what the page does with them; and that a name a person would use finds the thing even when it is not a
  control. A download link is graded too: the click leaves the page exactly where it was, and the offer
  is recorded with the address to fetch and the flag that asks the app to fetch it. Needs a Chromium; no app, no panel.
* `node scripts/browser-session-probe.mjs` — the same claims against the other two browsers, spoken over
  the ndjson the app really uses: the kept browser (`browser/session.mjs`) and the one-shot walk
  (`browser/driver.mjs`). The tool offers one verb list for three browsers, so a verb that is a no-op in
  one and real in another is a model being lied to about a page it cannot see. It grades polling, the
  measured `waited`, `navigate` mid-walk, where a walk that navigated says it ended up, choosing a
  `<select>` option in both browsers (and naming the choices when the name is wrong), and that each
  document's `health` counts start over — which is the Playwright half of the same rule the page bridge
  keeps.
* `./scripts/selftest.sh browser-panel` — the door, the column, and one page seen twice. The
  titlebar button opens a panel rather than starting a browser; a person types an address and gets
  a picture with real pixels and the page's own title; a `file://` address is refused by name and
  does not replace the page that was open; a reload is recorded as a person's act and takes another
  picture; then the model (`browse-session` in `tests/scenarios/scenarios.json`) opens the same
  page with `"session": true`, clicks in it, and its **second** call names no url at all — which
  only works if the browser stayed open between calls, and the mock refuses the turn unless the
  tool's own output carries the page title, `step click "More" ok` and `acted in the browser that
  was already open at`. The panel's url has to be a url the card itself names, which is the check
  that the column is not a picture of something else. Then the clock is on screen, and closing is
  two presses, and a closed browser leaves no picture behind. The column's own geometry is graded
  against the window: it takes a column of the window rather than adding height to it. Then the page
  is made to repaint by its own timer, and two reads of the picture a second and a bit apart have to
  differ — with the app's newest **picture id** having advanced too, so a panel that repaints from a
  frame it never counted cannot pass — and the live mark is read from the driver's answer rather than
  from the column's ask. A press in the *ground beside the page* — the letterbox a `contain`-drawn
  picture leaves when the box's aspect and the page's differ — must not reach the page, and must not be
  silent either: the column has to say what it declined. Then full window: the column has to reach the
  window's edges, the picture area has to be over 240px of height, and the page's own viewport has to
  be within a quarter of the aspect of that box — which is how "the column grew but the page is still
  letterboxed inside it" fails instead of passing — and Escape has to hand the column back.
* `./scripts/selftest.sh browser-inpage` — the page painted in this window, on its own board. It
  grades the rectangle first (the panel's box, the widget's allocation, the page's own viewport, the
  panel's complaint when they disagree), then both hands on one page: a person's press in the page and
  the model's click through `browse`, with the page's own endpoint counting the model's click and the
  paragraph saying what only a real click can say. Then the drawer's records by kind, a `#fragment`
  that moves the address without moving the page count, `back` that answers only once the page has
  moved, a reload that is not a second page, and the model reading the page's complaints through
  `page_log` — the tool's own reply naming the page, counting what it holds and giving the kind, which
  is what a model that merely narrated a log could not produce.

* `./scripts/selftest.sh browser-idle` — a second launch over a scratch `config.toml` that sets
  `[browser] idle_secs = 4`, because the span is what a person configures and a check that waits
  five minutes is a check nobody runs. The arm fails if the config did not reach the start
  (`idle_secs > 30`), and fails if the panel's sentence does not name *that* number: "it closed
  itself after 5 minutes" over a four-second wait would be a check passing by describing a setting
  nobody chose. After the app ends, the arm asks the process table whether any
  `browser/session.mjs` is still running — an app that closes its browser on a timeout and leaks
  one on exit has not answered the objection.
* `./scripts/selftest.sh --no-browser browser-panel` — the switched-off arm: the panel explains
  that keeping browsers is off and names the switch, instead of offering a box that can only fail;
  and hiding the column with nothing open takes the column away, while the titlebar button that
  explains why is still there. (This arm used to be green by accident — see `docs/BUILDRUN.md`
  Trap 138: it read the DOM in the same task as the state change, so on runs where the column was
  already open it was grading a leftover.)
* `./scripts/selftest.sh browse` — the one-shot tool, at a real page. The scenario (`browse-page`
  in `tests/scenarios/scenarios.json`) has the model click a button and evaluate an inline script,
  and its `require` list makes the mock endpoint refuse the turn unless the tool's own output
  contains the page's title, `click "More" ok`, `eval ok -> 5` and Chrome's real
  `Cannot read properties of undefined` text. `$base` is filled in by the mock with its own origin,
  so the URL is real whatever `MOCK_PORT` is. Then the check asks the endpoint how many times the
  page and its script were fetched — the one fact a narrated browser session cannot manufacture. It
  also insists the card ran in the mode the settings page says this start runs in, and gives the
  card the settings page's own reason string rather than a paraphrase. On a machine with no driver
  the group grades the other branch instead: the settings line must say the tool is not mounted and
  name both install commands.
* On a machine with no session driver the `browser-panel` group grades that branch instead: the
  panel must say the driver is not mounted and name both install commands.
