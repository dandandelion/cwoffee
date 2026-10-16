#!/usr/bin/env python3
"""Give a conversation a title, from a small model running on this machine.

The sidebar's title used to be the first line of the first message, cut at sixty
characters. That is honest, and useless for the one row you look at most: "hey can
you look at this" is a real first message and a terrible title. LiquidAI's
LFM2.5-230M is a 230 M-parameter model built for on-device CPU inference (42 tok/s on
a Raspberry Pi 5), which is small enough to name a conversation locally without
sending the first message to anybody.

It runs **out of process, and short-lived, on purpose**. cwoffee's desktop tree is
budgeted in the low hundreds of MB — app ~100, WebKit ~190, pi ~80. Loading torch plus
this model costs an order of magnitude more than that for a few seconds. A helper that
reads one prompt, prints one line, and exits hands that memory straight back; a
resident one keeps it for the whole session to serve one title per conversation, which
is not a trade worth making without measuring. So: prompt in on stdin, title out on
stdout, statistics on stderr, and no network at all once the weights are in the local
cache (see --allow-download).

Two things this is not:
  * It is not on the turn's critical path. Whoever calls it does so after the turn has
    settled, and renames the session when the answer arrives.
  * It is not required. If python, torch, or the weights are missing, the caller keeps
    the truncated title and says why. A title is decoration; a silent failure to
    decorate is not a bug, but a silent failure that *pretends* to have named the
    conversation is.

Usage:
  scripts/titlegen.py                     # one prompt on stdin -> one line on stdout
  scripts/titlegen.py --selftest          # the fixtures below, with timings
  scripts/titlegen.py --serve             # JSONL requests, model stays loaded
Read from the environment:
  CWOFFEE_TITLEGEN_MODEL   model id (default LiquidAI/LFM2.5-230M)
  CWOFFEE_TITLEGEN_CACHE   HF cache dir (default ~/.local/share/cwoffee/titlegen/models)
  CWOFFEE_TITLEGEN_THREADS torch threads (default 4)
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import time

MODEL_ID = os.environ.get("CWOFFEE_TITLEGEN_MODEL", "LiquidAI/LFM2.5-230M")
CACHE_DIR = os.environ.get(
    "CWOFFEE_TITLEGEN_CACHE",
    os.path.join(
        os.environ.get("XDG_DATA_HOME", os.path.join(os.path.expanduser("~"), ".local", "share")),
        "cwoffee",
        "titlegen",
        "models",
    ),
)

# The prompt, chosen by measuring four shapes over six real first-messages rather than
# by taste. What worked: one strict line, and a **quote primed onto the start of the
# answer** — the generation begins inside a quotation, which forces a noun phrase and
# gives a place to stop (the closing quote). What did not work: **few-shot examples**.
# They made this 230 M model verbose, and they leaked: given examples in English and
# German it answered an Italian message in English and an English one in German, and in
# one variant it copied an example title verbatim. A small model imitates the *pattern*
# of a shot list, and the pattern it takes away is not the one you meant.
#
# The language rule is stated because cwoffee's users write in several languages and a
# title translated out of the user's own words is a title they did not choose. Measured:
# with priming and no shots the Italian message came back "Fatturare".
SYSTEM = (
    "Name the conversation. 2 to 6 words. Same language as the message. "
    "A noun phrase, never a sentence."
)
PRIME = '"'  # the answer starts inside a quotation; the closing quote is the stop
MAX_NEW_TOKENS = 12

MAX_INPUT_CHARS = 700
MAX_TITLE_CHARS = 60
MAX_TITLE_WORDS = 8

# The selftest fixtures: (first message, words the title is expected to be about).
# Not a quality benchmark — a regression tripwire for "the helper stopped producing
# anything usable at all", which is the failure mode a caller can actually see.
FIXTURES = [
    ("Can you look at my quarterly cashflow spreadsheet and tell me why March is short?", ["cash", "quarterly", "spreadsheet"]),
    ("I need a python script that renames all the photos in a folder by their exif date", ["photo", "exif", "rename", "date"]),
    ("what would you plant in a shady backyard in ohio", ["plant", "shade", "garden", "backyard"]),
]

# A title that is really an answer wearing a title's clothes. The model is 230 M
# parameters and the card itself says it is not for reasoning-heavy work; priming keeps
# the shape, but the shape is not the whole of the task. These are the ways it slipped
# during the prompt measurements: an apology, a first-person answer, a promise to do the
# thing the message asked for. When a candidate matches, the caller keeps the truncated
# first line it already had: a dull true title beats a confident wrong one.
BAD_STARTS = (
    "i ", "i'", "im ", "we ", "you ", "your ", "here", "sure", "sorry", "yes", "no,",
    "no ", "certainly", "of course", "this ", "it ", "they ", "he ", "she ",
)
BAD_CONTAINS = (
    " i can", " i cannot", " i will", " i would", " as an ai", "i'm sorry", "i am sorry",
    "here's", "here is", "steps to", "you can ", "follow up",
)


def looks_like_an_answer(title: str) -> bool:
    low = title.lower().strip()
    if len(low) < 3:
        return True
    if any(low.startswith(b) for b in BAD_STARTS):
        return True
    if any(b in low for b in BAD_CONTAINS):
        return True
    # A title that ends in a preposition or a bare article was cut off mid-thought, which
    # means the model ran past the quote it was inside of. Show the whole phrase or do
    # not show the phrase at all.
    if low.endswith((" a", " an", " the", " to", " of", " in", " for", " and", " with", " that")):
        return True
    return False


def peak_rss_mb() -> float:
    """Peak resident set size of this process, in MB.

    VmHWM rather than VmRSS: what matters to cwoffee's memory budget is the high
    water mark the helper reaches while it loads, not what it happens to be holding
    when it prints.
    """
    try:
        with open("/proc/self/status", "r") as fh:
            for line in fh:
                if line.startswith("VmHWM:"):
                    return int(line.split()[1]) / 1024.0
    except OSError:
        pass
    return -1.0


def have_weights() -> bool:
    """Is the model already in the local cache?

    Checked by hand so the helper can refuse — in words — rather than reach across the
    network on a machine that may be on a phone link. Downloading 460 MB because a
    conversation wanted a title is not a decision the user made.
    """
    leaf = MODEL_ID.replace("/", "--")
    roots = [
        # Hugging Face's on-disk layout, spelled out rather than stumbled into: the walk
        # over CACHE_DIR alone would also say "cached" because of some other model.
        os.path.join(CACHE_DIR, f"models--{leaf}"),
        os.path.join(CACHE_DIR, "models", f"models--{leaf}"),
        os.path.join(CACHE_DIR, "models", leaf),
        os.path.join(CACHE_DIR, leaf),
        CACHE_DIR,
    ]
    for root in roots:
        if not os.path.isdir(root):
            continue
        if any(f.endswith(".safetensors") for _r, _d, fs in os.walk(root) for f in fs):
            return True
    return False


def sanitize(raw: str) -> str:
    """Reduce whatever the model printed to a title that is safe to put in a row.

    The model is 230 M parameters: it will occasionally echo a prefix ("Title:"), add a
    quote, run on, or answer the question instead of naming it. None of those belong in
    a sidebar, and a title that contains a newline would break the row it names.
    """
    text = raw.replace("<|im_end|>", " ").replace("<|endoftext|>", " ")
    text = text.splitlines()[0] if text.splitlines() else ""
    text = re.sub(r"^(title|conversation title|name)\s*[:\-—]\s*", "", text.strip(), flags=re.I)
    text = text.strip().strip("\"'“”‘’`«»").strip()
    text = re.sub(r"\s+", " ", text)
    # A conversation that opens with a question and gets an answer back is the most
    # likely way this fails: keep the naming clause and drop the sentence after it.
    text = re.split(r"[.!?。？！]\s", text)[0].strip()
    words = text.split(" ")
    if len(words) > MAX_TITLE_WORDS:
        text = " ".join(words[:MAX_TITLE_WORDS])
    if len(text) > MAX_TITLE_CHARS:
        cut = text[:MAX_TITLE_CHARS].rsplit(" ", 1)[0]
        text = cut.strip()
    return text


def load(threads: int, prefer_offline: bool):
    """Import torch and open the model. Seconds, not milliseconds — measure it."""
    import torch  # noqa: PLC0415  (import cost is the point: nothing pays it unless asked)
    from transformers import AutoModelForCausalLM, AutoTokenizer  # noqa: PLC0415

    torch.set_num_threads(max(1, threads))
    kwargs = {"cache_dir": CACHE_DIR}
    if prefer_offline:
        kwargs["local_files_only"] = True
    tok = AutoTokenizer.from_pretrained(MODEL_ID, **kwargs)
    model = AutoModelForCausalLM.from_pretrained(MODEL_ID, dtype="bfloat16", **kwargs)
    model.eval()
    return torch, tok, model


def make_title(torch, tok, model, text: str, max_tokens: int) -> tuple[str, dict]:
    """One title for one first message, and the timings that go with it.

    The generation is *primed*: the first token of the answer is a quotation mark we put
    there ourselves. That does two jobs a prompt sentence cannot — it makes the model
    open a noun phrase (a title is a quoted phrase, an answer is not), and it hands us a
    stop condition we can actually detect, the closing quote. Whatever follows the closing
    quote is discarded, because on this size of model the closing quote is where the
    naming ends and the talking resumes.
    """
    body = " ".join(text.split())[:MAX_INPUT_CHARS]
    messages = [
        {"role": "system", "content": SYSTEM},
        {"role": "user", "content": body},
    ]
    # transformers 5's chat template can hand back a dict wrapping the tensor; the model
    # card's own example indexes ["input_ids"]. Accept both rather than betting on which
    # release is installed on the user's machine.
    enc = tok.apply_chat_template(messages, add_generation_prompt=True, return_tensors="pt", tokenize=True)
    ids = enc["input_ids"] if not hasattr(enc, "shape") else enc
    primed = tok(PRIME, add_special_tokens=False)["input_ids"]
    if primed:
        ids = torch.cat([ids, torch.tensor([primed], dtype=ids.dtype)], dim=1)
    start = time.perf_counter()
    with torch.no_grad():
        out = model.generate(
            ids,
            do_sample=True,
            temperature=0.1,
            top_k=50,
            repetition_penalty=1.05,
            max_new_tokens=max_tokens,
        )
    gen_ms = (time.perf_counter() - start) * 1000.0
    # -len(primed): the primed quote is ours, so it is not part of what the model wrote.
    new = out[0][ids.shape[-1] - len(primed):]
    raw = tok.decode(new, skip_special_tokens=True)
    closed = raw.find(PRIME, 1) if raw.startswith(PRIME) else -1
    inside = raw[1:closed] if closed > 0 else raw[1:] if raw.startswith(PRIME) else raw
    quoted = closed > 0
    title = sanitize(inside)
    # Two kinds of failure get refused here rather than shipped to a sidebar row: an
    # answer wearing a title's shape, and a phrase that ran past the token budget with no
    # closing quote and ends mid-thought ("Build failed due to undefined reference to p").
    reason = ""
    if not title:
        reason = "empty"
    elif looks_like_an_answer(title):
        reason = "answer, not a name"
    elif not quoted and len(title.split()) >= MAX_TITLE_WORDS:
        reason = "ran past the budget with no closing quote"
    stats = {
        "prefill_tokens": int(ids.shape[-1]),
        "generated_tokens": int(new.shape[-1]),
        "gen_ms": round(gen_ms, 1),
        "quoted": quoted,
        "raw": raw.replace("\n", "\\n"),
        "title": "" if reason else title,
        "candidate": title,
        "rejected": reason,
    }
    return stats["title"], stats


def cmd_once(args) -> int:
    text = sys.stdin.read()
    if not text.strip():
        print(json.dumps({"ok": False, "error": "no text on stdin"}), file=sys.stderr)
        return 2
    if not args.allow_download and not have_weights():
        print(
            json.dumps(
                {
                    "ok": False,
                    "error": f"weights for {MODEL_ID} are not in {CACHE_DIR}",
                    "download": f'hf download {MODEL_ID} --cache-dir "{CACHE_DIR}"',
                }
            ),
            file=sys.stderr,
        )
        return 3
    t0 = time.perf_counter()
    torch, tok, model = load(args.threads, prefer_offline=have_weights())
    load_ms = (time.perf_counter() - t0) * 1000.0
    title, stats = make_title(torch, tok, model, text, args.max_tokens)
    stats.update(
        {
            "ok": bool(title),
            "load_ms": round(load_ms, 1),
            "peak_rss_mb": round(peak_rss_mb(), 1),
            "model": MODEL_ID,
        }
    )
    print(json.dumps(stats), file=sys.stderr)
    if not title:
        return 4
    sys.stdout.write(title + "\n")
    return 0


def cmd_serve(args) -> int:
    """Keep the model loaded and answer JSONL requests: {"text": "..."} -> {"title": ...}.

    Paid for with the model's memory for as long as the process lives. Left here because
    the alternative — paying the load cost per conversation — is worse on a slow disk, and
    because a caller that keeps this running owns an idle clock and a reaper, which is a
    decision for the caller to make out loud rather than a default smuggled in.
    """
    torch, tok, model = load(args.threads, prefer_offline=have_weights())
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except json.JSONDecodeError as exc:
            print(json.dumps({"ok": False, "error": f"not JSON: {exc}"}), flush=True)
            continue
        text = str(req.get("text", ""))
        if not text.strip():
            print(json.dumps({"ok": False, "error": "no text"}), flush=True)
            continue
        title, stats = make_title(torch, tok, model, text, args.max_tokens)
        stats["ok"] = bool(title)
        print(json.dumps(stats), flush=True)
    return 0


def cmd_check(args) -> int:
    """Availability in about a second, without loading the model.

    A settings panel, an installer and a gate all need to answer "is this feature actually
    usable here?". Doing that by loading 460 MB of weights freezes the panel and costs the
    whole price of the feature just to report on it, so this imports the two packages and
    looks at the cache, and nothing else.
    """
    out = {
        "model": MODEL_ID,
        "cache": CACHE_DIR,
        "weights_cached": have_weights(),
        "threads": args.threads,
    }
    try:
        import importlib.metadata as md

        out["torch"] = md.version("torch")
        out["transformers"] = md.version("transformers")
        out["torch_ok"] = True
    except Exception as e:  # noqa: BLE001 - the answer is the point, not the traceback
        out["torch_ok"] = False
        out["error"] = f"{type(e).__name__}: {e}"
    out["ok"] = bool(out["torch_ok"]) and out["weights_cached"]
    print(json.dumps(out))
    return 0 if out["ok"] else 2


def cmd_selftest(args) -> int:
    """Run the fixtures and print timings: what a caller would see, and what it costs."""
    t0 = time.perf_counter()
    torch, tok, model = load(args.threads, prefer_offline=have_weights())
    load_ms = (time.perf_counter() - t0) * 1000.0
    print(f"model={MODEL_ID} load_ms={load_ms:.0f} peak_rss_after_load_mb={peak_rss_mb():.0f}")
    bad = 0
    for text, want in FIXTURES:
        title, stats = make_title(torch, tok, model, text, args.max_tokens)
        hit = any(w.lower() in title.lower() for w in want)
        shown = title if title else f"(refused: {stats['rejected']} — {stats['candidate']!r})"
        print(f"  {'ok  ' if hit and title else 'miss'} {text[:46]!r} -> {shown} "
              f"(gen_ms={stats['gen_ms']} tokens={stats['generated_tokens']} quoted={stats['quoted']} peak_rss_mb={peak_rss_mb():.0f})")
        if not title:
            print(f"       !! nothing usable came out; raw was {stats['raw']!r}")
            bad += 1
        elif not hit:
            print(f"       note: none of {want} appear in the title")
    print(f"peak_rss_mb={peak_rss_mb():.0f} empty_titles={bad}")
    return 1 if bad else 0


def main() -> int:
    ap = argparse.ArgumentParser(description="name a conversation with a 230M local model")
    ap.add_argument("--threads", type=int, default=int(os.environ.get("CWOFFEE_TITLEGEN_THREADS", "4")),
                    help="torch CPU threads; bounded because the real model process and the UI share this box")
    ap.add_argument("--max-tokens", type=int, default=MAX_NEW_TOKENS)
    ap.add_argument("--allow-download", action="store_true",
                    help="let the helper fetch the weights if they are not cached (refuses without it)")
    ap.add_argument("--serve", action="store_true", help="stay up and answer JSONL requests")
    ap.add_argument("--selftest", action="store_true", help="run the fixtures with timings")
    ap.add_argument("--check", action="store_true",
                    help="print availability as JSON in about a second, without loading the weights")
    args = ap.parse_args()
    if args.check:
        return cmd_check(args)
    if args.selftest:
        return cmd_selftest(args)
    if args.serve:
        return cmd_serve(args)
    return cmd_once(args)


if __name__ == "__main__":
    sys.exit(main())
