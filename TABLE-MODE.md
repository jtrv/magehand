# Table Mode — design

Synthesis of a 6-lens design brainstorm (table social dynamics, voice
engineering, listener design, UI/sheets, architecture fit, prior-art skeptic;
~50 design points). This is the plan of record for the live-table layer.

## The contract

> **It is a very fast index card someone slides to the DM — not a fifth player.**

- The assistant **never speaks aloud and never initiates**. Hard rule, not a setting.
- **Players pull**: they ask (text box on their own phone), it answers privately,
  every answer footed with *"ask your DM to be sure."* Zero unsolicited output
  to players, ever.
- **The DM gets glanceable push**: silent cards on a DM-only screen, hard noise
  budget, one-tap dismiss. The tool proposes; only a human tap writes canon.
- The DM's spoken word always wins. When the tool is wrong, it loses the
  argument by design: cards carry their evidence (transcript quote + vault
  citation) and dismissing costs one second.

### What this reshapes from the original idea, and why

| Original idea | Verdict | Why |
|---|---|---|
| Respond to players via **audio** | **Cut.** Text on their own phone. | A synthesized voice either gets talked over or forces the table silent. History is unanimous: things that talk at tables die (Alexa D&D skills, AI-DM products). Conversational turn-taking needs ~1s; our floor is 3-5s. Text fails silently; audio fails socially. |
| LLM "waiting for signals it should respond" | **Reshaped**: it waits for signals it should *surface* — to the DM only. The only self-initiated output is a silent card. | Alarm fatigue is a one-way door: five wrong cards in the first hour and the DM never looks at the screen again. Precision over recall, everywhere. |
| Wake-word voice assistant | **Reshaped**: no wake-word engine. We transcribe everything anyway — detect the trigger phrase in the transcript text (fuzzy: "mage hand", "major hand"…). ~10 lines, zero deps, +1.5s latency nobody feels. | A second audio pipeline solving a problem we don't have. |
| Speaker diarization ("who said that") | **Skipped in v1.** Attribute nothing. | Real-room diarization over one far-field mic with crosstalk, dice, and funny NPC voices runs 20-35% error — 1 in 4 lines misattributed. Keying per-player secrets off that is how you leak a spoiler to the wrong player. The DM was in the room; they know who spoke. |
| Cheapest/fastest LLM | **Answered as a cascade** (below): tier-0 is not an LLM at all; tier-1 is a ~4B local model; tier-2 is the existing ask pipeline. All-local: ~$0/session. Worst-case all-cloud: <$2.50/session. Cost is a rounding error next to the pizza — the design optimizes precision and privacy instead. | |

## System shape

```
[conference mic puck] → whisper.cpp (child process, VAD 5-10s chunks,
     hotworded with the vault's proper nouns)
        → append-only live transcript file in the vault
             ├── REACTIVE loop (per utterance): tier-0 lexicon scan
             │      ├── trigger phrase → tier-2 ask/--player → player's phone
             │      └── entity/dispute/sheet hits → tier-1 classify → DM cards
             ├── DIGEST loop (every ~3 min): tier-1 over window + open
             │      threads' trigger: lines → thread nudges + draft facts
             └── session end → cleanup pass → existing `magehand log`
                    canon extraction → recap/threads/loot as today
[magehand serve] tiny_http + SSE:  /dm (cards, tap actions)   /p/<token> (per player)
```

## Voice pipeline

- **Mic**: one USB conference puck ($80-130, Anker PowerConf / Jabra class) at
  table center. Onboard DSP (AGC, noise suppression) moves accuracy more than
  any model choice. Hard no on per-player mics or phone meshes — player setup
  friction is forbidden. Optional later: wireless lav for the DM only.
- **STT**: local whisper.cpp (large-v3-turbo or distil), Silero-VAD-gated
  5-10s utterance chunks, spawned as a **child process** whose stdout we parse
  (~30 lines; a crashed child gets respawned by a 5-line watchdog). No Rust
  binding, no daemon, no token streaming.
- **Hotwording is load-bearing**: seed whisper's initial_prompt with the
  campaign's proper nouns — NPCs, places, threads — straight from the index.
  "Zephyrine of Vhalarath" mis-transcribed is the #1 source of
  confidently-wrong cards, and only we have this lexicon.
- **Local-only audio, always**: the transcript is the asset (it feeds canon
  extraction); venue wifi dying must not cost the session record. Raw audio is
  never persisted past the ~30s transcription buffer.
- Expect 8-12% word error on clean turns, 20%+ during crosstalk. Every
  downstream consumer must tolerate a lossy transcript.

## The listener

**Signal taxonomy** — six classes, each with a fixed route and audience:

| # | Signal | Detected by | Routed to | Notes |
|---|---|---|---|---|
| 1 | INVOKE ("hey magehand …") | tier-0 phrase match | tier-2 `ask --player` → asking player's phone | the ONLY player-facing output |
| 2 | RULES-DISPUTE ("wait, can I…", "rules say…") | tier-0 regex → tier-1 confirm | tier-2 ask (rulebook + rulings tiers) → DM card | DM chooses to read it aloud |
| 3 | ENTITY-MENTION (NPC/place/item name) | tier-0 lexicon only | NPC card to DM | no LLM at all |
| 4 | TRIGGER-MATCH (thread `trigger:` lines) | digest loop, tier-1 | DM nudge | semantic; 3-min lag fine |
| 5 | FACT ("we killed Grol", prices, promises) | digest loop, tier-1 | draft session log | reviewed at session end |
| 6 | SHEET-EVENT ("I take 12") | tier-0 regex → tier-1 | suggested sheet delta, DM ticker | never auto-applied |

**Tier cascade**:

- **Tier-0 (free, µs)**: the FTS index's own titles + file stems of
  npcs/threads/statblocks = a ~200-entry campaign-specific lexicon we already
  maintain by writing markdown. Lowercase substring scan per utterance, plus
  ~20 hardcoded regexes (wake phrase, dispute phrasing, damage patterns).
  Tier-0 gates tier-1: the LLM only ever judges pre-detected candidates —
  that's false-positive hygiene, not cost control.
- **Tier-1 (cheap classify)**: local **Qwen3-4B-class via Ollama** (~150-400ms
  on any GPU), temperature 0, strict JSON `{signal, entity, confidence}`.
  Closed-set classification of pre-detected candidates is exactly what 4B
  models are reliable at. Cloud fallback (flash-lite/Haiku-class via the
  existing OpenAI-compatible config) is a config change, not code.
- **Tier-2 (full answer)**: the existing `ask`/`--player` pipeline, unchanged,
  reached ONLY from INVOKE and RULES-DISPUTE. One addition for live use:
  prepend the last ~10 transcript lines so "can I do that again?" resolves.

**Two loops**: reactive (per utterance, signals 1/2/3/6, ~40s context window)
and digest (every ~3 min, signals 4/5, with all open threads' `trigger:` lines
pasted verbatim — ~500 tokens, no retrieval needed).

**Cost & latency, honestly** (4h session ≈ 30k words ≈ 40k transcript tokens):
- All-local: **~$0**, needs ~6-10GB RAM/VRAM total alongside whisper.
- All-cloud: STT ~$1.40 + tier-1 ~$0.05-0.10 + tier-2 (~25 calls) ~$0.15-0.45
  = **under $2.50/session**.
- Latency: entity card <50ms; DM nudges 0.5s-3min (deliberately unhurried);
  player INVOKE answer 3-7s end-to-end with a "heard you" flash at ~1.5s
  (the flash is load-bearing — silent seconds make players repeat themselves).

**False-positive policy — asymmetric by audience**: player channel has
precision ≈ 1.0 by construction (output only follows an explicit ask). DM
channel tolerates junk, controlled by plumbing not ML: dedupe per entity per
10 min, ≤1 pushed card/30s (overflow collapses to a badge), confidence <0.6
dropped from live view but kept for the post-session digest. Target ≤10 live
cards per session; instrument the DM's dismiss rate from day one. No learning
loop — dedupe and rate limits solve this at table scale.

## DM surface

Web page on a tablet propped behind the DM screen (physically angled away —
it shows secrets; screen angle is doing real security work). Three zones:

1. **Pinned strip**: initiative + party HP pips + conditions, threads past due.
2. **Card feed**: newest on top, color by class (secret=purple, rules=blue,
   reminder=amber, consequence=red), ≤10-word headlines, ≥24px type readable
   at arm's length. Cards auto-fade after 5 unread minutes (everything is
   still in the session file — the feed must never become an inbox).
3. **Transcript ribbon**: collapsed to the last utterance, tap to expand.

Every card: ≤3 buttons pre-bound to existing verbs — **Send to <player>**,
**Save ruling**, **Open thread** — plus swipe-dismiss. No confirmations;
10-second undo toast instead (append-only files make undo one line). Nothing
writes to the vault without a tap.

v0 of this surface is the terminal `magehand listen` already runs in.

## Player surface

QR code on the table → LAN web page. Zero installs, zero accounts: per-player
capability URL (`/p/<128-bit-token>`) minted at serve start — scanning IS the
login; cookie pins it. Token maps to the player slug, which scopes secrets and
backstory retrieval server-side (the existing `--player`/visibility machinery —
the spoiler boundary stays structural, never prompt-dependent).

Three tabs, portrait, dark theme: **SHEET** (live HP/slots/conditions with big
± buttons) · **ASK** (existing spoiler-safe ask; answers private, non-binding
footer, ~10 asks/player/hour rate limit) · **STUFF** (their secrets and
handouts, latest recap pinned). No player-to-player chat, no dice roller, no
notifications. The phone must stay less interesting than the table.

Identity is spoofable-by-friends by design; a per-player PIN is the one-line
upgrade if a table ever cares.

## Character sheets

**Refuse the character builder.** D&D Beyond owns building and leveling; the
rules logic is months of work plus a licensing minefield, and it solves the
wrong half.

Sheets are vault markdown (`sources/campaign/sheets/<pc>.md`): YAML frontmatter
holds the ~20 machine-touched numbers (AC, HP/temp/max, speed, slots per level,
death saves, conditions, ability mods, attacks as one-liners); the body below is
freeform prose the machine never writes. Phone ± buttons rewrite frontmatter
keys only and re-index — so the DM dashboard gets a live party health strip,
the listener can cross-check ("transcript says Torvin dropped; sheet shows
22 HP?"), and hooks/catchup/canon extraction gain ground truth. Obsidian-
compatible and printable throughout.

Level-ups: `magehand sheet import <pc>` — paste anything (D&D Beyond copy
text, OCR dump), one cheap LLM call fills the frontmatter, diff shown before
writing. ~4×/year/character.

## Architecture

- **Two new commands** in this repo, matching its flat blocking style:
  - `magehand listen` — spawns whisper, appends the live transcript to
    `sources/campaign/sessions/<date>-live.md` (fsync per utterance; excluded
    from FTS via one glob rule until promoted), runs both listener loops,
    prints cards to the terminal.
  - `magehand serve` — **tiny_http + hand-rolled SSE** (~150 lines,
    thread-per-connection; 7 clients on a LAN is nothing). NOT axum: the
    codebase is blocking ureq/rusqlite, and tokio would force an async seam
    through everything for zero benefit at this scale. Ponytail ceiling named:
    migrate if this ever serves many tables.
  - UI = two hand-written HTML files (dm.html, player.html), vanilla JS +
    EventSource, embedded via include_str!. No npm, no build step; the whole
    product ships in the binary.
- **Cards** append to `.magehand/live/cards.jsonl` (crash recovery, audit
  trail, tail-able debugging) and push over SSE. "Accept" POSTs call the
  existing pub(crate) command internals directly — the live layer is a thin
  event source over verbs that already exist.
- **Session end** ("End Session" button + SIGINT handler): stop whisper, one
  cleanup LLM pass over the raw transcript (ASR mishearings must not pollute
  canon), promote the file, run the existing `log` canon extraction, ingest.
  The end state is byte-identical in kind to a hand-typed session log — recap,
  catchup, prep, thread aging all work unchanged. Keep the raw `-live.md`
  forever; re-run extraction after prompt tweaks.
- **Privacy enforced in code, not README**: if the listener's LLM endpoint is
  not localhost, its retrieval context is built with the `--player` filter and
  secret-touched cards are disabled. Cloud listener mode = no secrets in
  prompts, period (an explicit opt-in flag can override). This is the honest
  pressure toward a local 4-8B for the listener while OpenRouter handles the
  heavyweight ask/prep/log calls.

## Consent & rollout

- Recording is a **visible ritual**: one plain sentence at session zero
  ("a local program transcribes so I don't take notes; audio is deleted as
  it's transcribed; nothing leaves this laptop" — literally true when local),
  a big REC indicator on the DM page, and a **table-break pause hotkey** —
  out-of-game venting must not become canon. Any player can have a line
  struck: it's a text edit.
- **Shadow mode is the deployment plan**: Session 1 transcribes and logs every
  would-be card to a file, surfaces nothing live; the DM grades the card log
  afterward (~15 min) — a labeled precision dataset at zero social risk, and
  the night still pays off via auto-scribed canon extraction. Session 2
  enables the two highest-precision classes. Full drawer by session 3-4.
  Go-live gate: ≥80% of shadow cards rated useful, two sessions running.
- The player QR page (sheet + recap, then ask) can ship from session 1 —
  pull-only can't misfire socially.

## Refuse list (permanent, per the skeptic)

No VTT (maps, tokens, fog of war). No dice engine; no combat automation
(auto-resolving attacks would encode RAW back in — the rulings system is the
soul of this tool). No character builder. No TTS as a default channel (one
future exception allowed: DM-triggered playback of pre-approved boxed text).
No diarization until a real need survives three sessions. No multi-tenant
hosting, no mobile apps, no JS frameworks, no accounts. The scribe features
(transcribe→recap) are a commodity six SaaS products already sell — our moat
is the **canon-grounded listener**: "you ruled this differently in session 12"
and "the party is one question from the twin's secret" are impossible without
this vault.

## Phases

| Phase | What | Effort | Independently useful because |
|---|---|---|---|
| 1 | `magehand listen`: whisper child, hotworded live transcript into the vault, terminal print, SIGINT → cleanup → existing `log` extraction | ~1-2 weekends, ~250 LOC, 0 new deps | auto-scribe into YOUR canon pipeline; validates STT at a real table before anything else exists |
| 2 | Listener loops + cards.jsonl + terminal card feed (shadow mode) | ~1-2 weekends, ~400 LOC | proves signal precision with the cheapest consumer |
| 3 | `magehand serve`: tiny_http + SSE, dm.html with tap actions | ~1-2 weekends, ~500 LOC + tiny_http | the glanceable DM surface |
| 4 | Player tokens + QR + player.html (sheet/ask/stuff) + frontmatter sheets + `sheet import` | ~1-2 weekends, ~400 LOC + qrcode crate | the zero-friction player layer |

Play real sessions between phases; every failure mode in this domain is only
observable at an actual table. Total: roughly doubles the current codebase,
2-3 new deps.

## Decisions (2026-07-17)

1. **Tier-1 runs cloud** (via the existing OpenRouter config). Consequence per
   the privacy rule: **secret-touched cards are disabled by default** — they
   only exist with local tier-1, or behind an explicit
   `MAGEHAND_CLOUD_SECRETS=1` opt-in. STT stays local regardless.
2. **DM laptop is an Apple Silicon MacBook, laptop mic to start.** M-series
   MacBooks carry a good three-mic beamforming array — test it at a real
   table before buying anything. Cheap upgrade path if WER disappoints:
   $30-60 budget conference pucks (eMeet/TONOR class) capture most of the
   $100 puck's benefit. Metal-accelerated whisper.cpp runs large-v3-turbo
   comfortably real-time on M-chips.
3. **Table is phones-flexible** — Phase 4 player pages are viable as designed.
4. Still open: **wake phrase** — test "hey magehand" transcription reliability
   during Phase 1 shadow sessions.
