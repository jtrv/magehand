# Magehand

Tabletop RPG assistant for DMs: local RAG over your rulebooks **and your campaign**,
answered by an LLM (OpenRouter or a local Ollama model). Campaign state lives as a
plain-markdown Obsidian vault that magehand both writes and reads.

## Quickstart

```sh
cargo build --release
./target/release/magehand ingest          # index everything under sources/
./target/release/magehand ask "does grappling stop opportunity attacks?"
./target/release/magehand chat            # interactive session for game night
```

`magehand search <terms>` shows raw retrieval hits (useful to sanity-check the index).

## Campaign commands

| Command | What it does |
|---|---|
| `log <file\|-\|text>` | Archive raw session notes: extracts events/NPCs/debts/loot into a structured session record, flags contradictions with canon, suggests threads |
| `recap` | Player-safe "previously on…" + DM brief (threads to surface, states changed, debts due) |
| `ruling <text>` | Record a table ruling — future answers lead with it over RAW |
| `npc <desc> [--save]` | NPC consistent with your canon (no name collisions, hook wired to open threads); `--save` canonizes it |
| `thread add/list/close` | Chekhov ledger: open plot loops, sorted staleness, fed into recap/prep |
| `prep <chapter/topic>` | One-page runsheet: scenes, DCs, stat block pointers, read-alouds, contingencies |
| `lint <file>` | Check draft prep notes against established canon before game night |
| `yesand <question> [--commit]` | Say yes to in-world questions without breaking canon; `--commit` records the new fact |
| `consequence <event> [--save]` | Immediate fallout + delayed consequences; `--save` files them as trigger-tagged threads |
| `boxtext <scene>` | Read-aloud text built only from details in your sources, citations listed |
| `name <culture/kind>` | Setting-consistent names, deduped against your existing NPCs |
| `hooks <next session topic>` | One backstory tie-in per character (drop backstories in `campaign/backstories/`) |
| `secret add <player> <text>` / `secret list [player]` | Who-knows-what ledger, DM-only |
| `catchup <player> [missed]` | Player-safe brief for absent players (their own secrets woven in, others' never read) |
| `onboard` | One-page primer for a new player, from player-visible material only |
| `spotlight` | Who's been quiet lately + one suggested scene per neglected character |
| `time [advance <d> \| set <d>]` | In-world calendar; advancing announces open threads whose `--due` day arrived |
| `timeline` | Campaign chronology from session records |
| `loot list/add/fund` | Party loot + fund ledger (gp/sp/cp), `--secret` for unidentified-item truths |
| `downtime <activity> [--commit]` | Resolve by whatever downtime rules you've ingested; commit charges the fund, advances the calendar, and logs it |
| `statblock <name> [--save]` | One-screen play crib for any monster in your books |
| `statblock --stub <concept> [--save]` | Homebrew stat block drafted from comparables in your books |
| `--player` | Spoiler-safe mode for `ask`/`chat`/`search`: retrieval restricted to player-visible sources |

The calendar is a plain day counter in `campaign/calendar.md`; add a `months:`
line there for named fantasy dates (any homebrew calendar works). Session logs,
loot, and downtime entries are stamped with the in-world day automatically.

## Table mode (Phase 1 — live transcript)

`magehand listen` transcribes game night into the vault and prints tier-0
entity cards (NPC/thread/statblock mentions, matched against your own vault —
no LLM calls during play, and reading/saving the transcript never waits on the
LLM). **Ctrl-C ends the session**: one LLM cleanup pass fixes mishearings using
your campaign's proper nouns and writes a **reviewable draft** to
`.magehand/live/<date>-session-draft.md` — nothing enters canon automatically.
Read the draft, then `magehand log <that path>` promotes it (recap, threads,
and contradiction checks then work as if you'd typed the notes). The raw
transcript is kept (`sessions/<date>-live-sNNN.md`, excluded from the index).

Setup (macOS / Apple Silicon):

```sh
brew install whisper-cpp
# grab a model — small.en to start, large-v3-turbo runs fine on M-chips:
curl -L -o models/ggml-small.en.bin \
  https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.en.bin
magehand listen
```

The STT command is a template you own via `MAGEHAND_STT_CMD` (default:
`whisper-stream -m models/ggml-small.en.bin --step 0 --length 8000 -vth 0.6 -t 4`).
`{names}` in the template is replaced with your campaign's proper nouns — if
your whisper build supports `--prompt`, add `--prompt "{names}"` to cut
fantasy-name mishearings dramatically. `MAGEHAND_STT_VERBOSE=1` shows whisper's
own output. `magehand listen --stdin` runs the whole pipeline from typed/piped
lines — useful for testing and for any STT that can print lines.

### Phase 2 — the signal listener

While `listen` runs, a canon-grounded listener watches the transcript and emits
**cards**: `[rules]` (a detected rules dispute, answered table-speed with
citations — house rulings override RAW), `[trigger]` (an open thread's
`trigger:` condition just happened at the table), `[fact]` (canon-worthy events
— deals, prices, promises, deaths), and `[secret]` (play is near a per-player
secret — only with a local LLM endpoint or `MAGEHAND_CLOUD_SECRETS=1`).

Every card lands in `.magehand/live/cards-<date>.jsonl`; live printing obeys a
noise budget (30s gap, confidence floor, 10-min dedupe). **Run your first real
session with `--shadow`** — cards are logged but never shown — then grade the
log afterward with `magehand cards`: which would you have wanted mid-game?

| Env var | Purpose |
|---|---|
| `MAGEHAND_LISTEN_MODEL` | Cheap classifier model for the listener (defaults to your main model) |
| `MAGEHAND_DIGEST_SECS` | Trigger/fact sweep interval (default 180) |
| `MAGEHAND_CLOUD_SECRETS` | Opt in to secret detection on a non-local endpoint |

### Phase 3 — the DM dashboard

`magehand serve` puts the card feed on a web page for a tablet propped behind
the DM screen. Run it alongside `listen` (the card JSONL is the bus between
them, so a dashboard crash never touches transcription):

```sh
magehand listen &     # transcribes + emits cards
magehand serve        # serves the dashboard; prints a tokenized URL
```

It prints a localhost link and a LAN link, each carrying a **capability token**
(the page shows secrets, so access is gated — no token, no dashboard, even on
shared wifi). Open the LAN link on a tablet: three zones — a pinned strip of
open threads (overdue ones flagged), the live card feed (newest on top, colored
by type, auto-fading after 5 min), and a transcript ribbon. Each card has
one-tap actions wired to existing commands: **Save ruling** (rules cards →
`ruling`), **Open thread** (fact/trigger cards → `thread add`), **Dismiss**.
Nothing writes to the vault without a tap. `--port N` overrides the default 7979.

### Phase 4 — player pages + character sheets

`magehand serve` also hosts a zero-install player surface. Its startup prints a
**join page** URL (`/join?t=…`, DM-only); open it on the laptop and it shows one
**QR code per player**. A player scans theirs once — that capability URL *is*
their login (a cookie pins it), no app, no account. Their phone page has three
tabs: **Sheet** (live HP / slots / conditions with big ± buttons), **Ask**
(spoiler-safe rules/lore Q&A over the `--player` retrieval, rate-limited, every
answer footed "ask your DM to be sure"), and **Stuff** (their own secrets and
the latest recap). Each player sees **only their own** secrets — the token maps
to one player and scopes retrieval server-side.

Character sheets are vault markdown (`sources/campaign/sheets/<slug>.md`): the
~two dozen numbers that change at the table live in the frontmatter (the phone's
± buttons edit those); everything else is freeform prose below the fence, edited
in Obsidian. Not a character builder — build and level in D&D Beyond or on paper.

```sh
magehand sheet new "Mira Quickfingers"   # scaffold a blank sheet
magehand sheet import "Mira" -            # fill it from pasted D&D Beyond text (stdin)
```

**Slug consistency:** a player is identified by their file slug, so their sheet,
backstory (`backstories/<slug>.md`), and secrets (`secrets/<slug>.md`) must
share one — name the sheet to match (e.g. all three `mira.md`) or the roster
treats them as different people.

Full design: `TABLE-MODE.md`. Start with the MacBook's built-in mics; a $30-60
conference puck (eMeet/TONOR class) is the cheap upgrade if a real session's
transcript looks rough.

Write-back commands re-index automatically; answers get labeled excerpts with
precedence **house rules > campaign canon > rulebooks**, and conflicts are flagged.

## The vault (Obsidian)

`sources/campaign/` is the campaign store *and* an Obsidian vault — open that folder
in Obsidian to browse sessions, NPCs, threads, and recaps. Generated notes carry
frontmatter and `[[wikilinks]]`, so graph view maps your campaign; anything you edit
or add by hand is picked up on the next ingest. `sources/house/` holds house rules
and recorded rulings (player-visible, override RAW in answers).

Visibility for `--player` mode: rulebooks and `sources/house/` are player-visible;
`sources/campaign/` is DM-only except `recaps/` and `public/` (`recap` splits its
output: player text to `recaps/`, DM brief to `briefs/`); drop spoiler-heavy
adventure books in `sources/dm/`. Matching is case-insensitive, and stray files at
the `sources/` root fail closed to DM-only.

Set `MAGEHAND_UTC_OFFSET` (hours, e.g. `-7`) so late-night sessions aren't stamped
with tomorrow's UTC date.

## Sources

Drop rulebooks into `sources/` and re-run `ingest`. Supported: `.md`, `.txt`, `.json`
(arrays of entities, e.g. [5e-bits/5e-database](https://github.com/5ebits/5e-database)
exports), `.pdf` (text-based, not scanned).

Included out of the box: the **SRD 5.1** as markdown (CC-BY 4.0, cloned from
[OldManUmby/DND.SRD.Wiki](https://github.com/OldManUmby/DND.SRD.Wiki)). If you
re-clone it, delete the `*_A-Z` aggregate dirs (`Spells_A-Z`, `Monsters_A-Z`,
`Magic_Items_A-Z`) — they duplicate the per-entity files and waste retrieval
slots on doubles. The
**SRD 5.2.1** (2024 rules) is also CC-BY 4.0 — grab the PDF from Wizards' site and
drop it in. For anything non-free (PHB, DMG, adventures): only ingest books you own;
the index stays on your machine, but the excerpts are sent to whichever LLM API you
configure, so use a local model for content you'd rather keep offline.

## Configuration

| Env var | Default | Notes |
|---|---|---|
| `OPENROUTER_API_KEY` | — | If set, OpenRouter is the backend |
| `MAGEHAND_BASE_URL` | OpenRouter, else `http://localhost:11434/v1` | Any OpenAI-compatible endpoint (Ollama, LM Studio, vLLM…) |
| `MAGEHAND_MODEL` | `openrouter/auto`, else `llama3.1` | |
| `MAGEHAND_API_KEY` | `OPENROUTER_API_KEY`, else `ollama` | |

## How it works

Ingest chunks documents by heading (markdown), entity (JSON), or paragraph (PDF/txt)
into a SQLite **FTS5** index — BM25 keyword search, title-weighted. At question time,
one cheap LLM call expands your question into rulebook terms (so "can I hit someone
running past me" finds *opportunity attacks*), the top 8 chunks go into the prompt,
and the model answers with section citations.

No vector DB on purpose: the corpus is small and D&D terminology is distinctive, so
BM25 + query expansion covers it. If retrieval ever feels thin, the upgrade path is
hybrid search via `sqlite-vec` + a local embedding model.

Known limitation: the FTS5 tokenizer is word-based, so CJK rulebook translations are
effectively unsearchable (switch the table to the `trigram` tokenizer if you need that).

The DM persona lives in `SYSTEM_PROMPT` in `src/main.rs` — tune it to your table.
