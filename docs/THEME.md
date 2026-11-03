# Theme: one stored choice, two homes

README used to admit this one: `config.rs` had `theme = light | dark | system`, the settings screen
wrote it, and **nothing read it**. The palette lived in `ui/src/App.svelte`'s own
`localStorage['cwoffee.theme.v1']`, and `matchMedia('(prefers-color-scheme: dark)')` was read once at
boot with no listener. That is one setting pretending to be two, and both halves were wrong:

- the file's value was a fact nobody acted on, so a person editing `config.toml` changed nothing;
- the window's value was the app's setting in practice but a per-browser storage entry in fact, so a
  desktop, a browser on another port and a phone had three coincidences called "the theme".

## Where each value lives, and who may change it

| | home | who writes it | who reads it |
|---|---|---|---|
| the stored choice | `config.toml`, top-level `theme` | `set_theme` (drive role only) | every shell at start: the window, a gateway tab, the terminal |
| a window override | that window's own store | the window itself, and it says *override* while doing it | that window only |
| `system` | not a palette — a subscription | nobody; each client's own media query | the client that has one |

Three rules come out of that table, and each is a thing a check enforces rather than a thing the UI
promises:

- **A per-window override is allowed, but only when the chrome says the word.** The Appearance page
  has two rows — "Colours, stored on this host" and "This window only" — and the second prints
  `data-theme-override` and `data-theme-source`. A control that looks like the setting and isn't it
  is exactly the failure the split exists to prevent.
- **An override never rewrites the stored choice.** A phone tab that could write `theme` would
  repaint the desktop, so `set_theme` is drive-only on the gateway and the override is passed to
  `theme_words` as *an argument for the sentence*, never as a write. The check sets an override and
  then reads `config.toml` to prove it still says what the host stores.
- **`system` resolves per client, so two surfaces can disagree and both be obeying the one value.**
  The engine's own words for that are in `crates/engine/src/theme.rs` (`setting_note`, `about_says`),
  published on `HostState` as `theme_note` / `theme_about`, so a settings row on a phone and one on
  the desktop cannot paraphrase the two homes differently. About prints the line.

`theme_words` exists because the *sentence* is the engine's business while the *override* is the
client's. A client that wrote its override into the host's value would be one window changing
everybody else's setting; a client that phrased the About line itself would be a second answer to
"which palette is whose".

## `system`, live — and what this machine cannot observe

The listener is `matchMedia('(prefers-color-scheme: dark)')` with a `change` handler, attached for
the window's life and removed on teardown, mirroring the existing `prefers-reduced-motion`
subscription in the same file. The Appearance page reports whether it is attached
(`data-theme-listening`), because "follows the system" is a claim about a live subscription and a
snapshot taken at boot is not one.

The arm tries to synthesize a `MediaQueryListEvent`, dispatch it, and read the paint. On this
machine that worked: the run's own ok-line ends with *"the media query was observed following"*. Where
an engine refuses to synthesize the event, the check prints a skip note and grades only the listener's
presence and the explicit arms (Light / Dark / System / Follow-the-host), and the ok-line says which
half it measured. A claim a check cannot enforce stays unclaimed on that machine — the same discipline
that stops `explore` from saying a server is mounted when no call can reach it.

A repaint is also a re-layout, and `docs/TESTING.md` already carries the reload-painting lesson: a
screen that changes while a person watches an answer arrive costs that answer its streaming. So a
palette change asked for **during a turn waits for the turn to end**, and says so in a note. The
check sends a turn, changes the palette mid-turn, asserts the turn is still running, and asserts the
paint landed afterwards — a deferred repaint that silently never happened would read as a broken
theme.

## Light is a theme, not a fallback

PI-CORE-PLAN lesson 132 is the whole reason this section exists: *"lighter" is not a direction both
themes share*. `--color-raise` sits deeper than a near-white screen and paler than a near-black one,
and `paper-2` is what form controls wear — so a menu wearing `paper-2` separates from its own
trigger and vanishes into the page behind it.

The floating surfaces — footer menu (`.pop`), notices (`.toast`), the approval card, the preview
overlay (`.pv-head` / `.pv-body`), the revert card, the tree panel, the file ledger, and the narrow
drawer — now all wear `--color-raise`, and the *separation* is a token too:

```
:root            --lift: 0 10px 28px rgba(60, 50, 30, 0.14);   --scrim-edge: rgba(60,50,30,.16)
[data-theme=dark] --lift: 0 8px 22px rgba(0,0,0,.55), 0 0 0 1px rgba(233,230,222,.07);
                 --scrim-edge: rgba(233,230,222,.12)
```

A black drop is invisible on a near-black ground, and a black dim over a near-black desk reads as
nothing at all — so the drawer's edge fade and the scrim behind it became theme-named tokens rather
than literals. The old `.sidebar::after` gradient was `rgba(0, 0, 0, 0.16)`, which is the light
theme's direction wearing the dark theme's name.

The check grades **numbers, not colour names**: for each surface, in each palette, the resolved
background's lightness against the surface behind it (oklab L scaled to the same 0..255 ruler this
engine answers in — the same helper the open-row check uses, for the same reason: a comparison that
silently becomes `NaN` compares nothing and congratulates itself). A surface within 6 of the surface
behind it and declaring no shadow fails, in either palette.

## The arm that makes the light theme bite

`selftest.sh --theme light|dark|system` sets `CWOFFEE_SELFTEST_THEME`, which the armed run reads from
`selftest_config` and applies as a **window override** — it does not rewrite the host's stored value,
because the arm is grading a paint and not changing a setting. One run paints one palette, so:

- the detached probes grade both palettes' surface rules in *either* run;
- the **live** claim — "the reading surface under a light name measures above 150 of 255" — is only
  gradeable in a run that is painted in that palette.

A full pass therefore launches the `theme` group a second time in the light palette and folds that
arm's verdict into the run's result.

**The sabotage, run and measured.** `background: #1e1d1b` — the dark palette's paper, hard-coded into
a surface rule both palettes wear — and the light arm said:

```
SELFTEST FAIL: .pop (the footer menu) puts the light palette's ink (rgb(43, 42, 39), L=6) on
rgb(30, 29, 27) — contrast 1.98, under 4.5. A floating surface wearing the other theme reads as a
bug in the theme, not as a surface.
```

The ink-on-surface number is the half that catches it. That surface *separates* from a near-white page
by 223 of lightness, so a separation-only check passes it, and the dark run passes it too because the
dark ink is legible on it. Contrast is the number that says "this surface belongs to the other
theme", and it is not a colour name — which is lesson 132 said as arithmetic.

The check had a bug of its own on the way there, and it is worth keeping: the ink was measured
*outside* the palette switch, so the dark loop compared a dark surface against the light palette's ink
and reported `contrast 1.00` — a failure that meant nothing. Everything for one palette is now read
inside one `withPalette` switch, and the comment in the check says what the wrong version printed.

## Retired keys stay retired

`theme_loud` and `theme_aggressive` are not vocabulary. `theme_loud` keeps its behaviour (it is not a
pref, and a write to it is refused in words naming the key), and `theme_aggressive` stays a retired
key that refuses; `prefs.rs`'s table is still the whole vocabulary, so a drawer any client can write
into stays impossible. `Theme::parse` refuses both spellings too — a word that is not a palette is a
typo, and `theme` is the setting that used to be *unread*, which made a typo and an ignored key look
identical from outside.
