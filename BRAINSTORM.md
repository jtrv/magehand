# Magehand roadmap brainstorm — DM helper Pareto set

Synthesis of a 6-lens brainstorm (prep, live-combat, improv, continuity,
player-facing, existing-tools skeptic; 48 raw ideas). Ranked by
friction-removed-per-session ÷ build effort on the current CLI+SQLite+RAG
architecture.

## The core thesis

Rules lookup is only half-solved territory (D&D Beyond / 5e.tools cover
official content). The genuinely unserved gap every lens converged on is
**campaign memory**: RAG over the DM's *own accumulating campaign state*, with
**write-back** — improvised content becoming queryable canon automatically.
Kanka/World Anvil/LegendKeeper/Obsidian are all hand-maintained wikis; the DM
does the extraction, filing, and cross-referencing. That clerical loop is the
boring part, and a local LLM over a local index is the right tool to delete it.

Two architectural primitives unlock ~80% of everything below:

1. **Tagged sources** — a `tier`/`visibility`/`kind` column on chunks
   (rulebook vs house-rules vs campaign-notes vs backstory; dm-only vs
   player-visible).
2. **Write-back** — commands that append generated/observed facts to
   `sources/campaign/` (or SQLite tables) and re-index, so canon accretes.

## Tier 1 — the 20% that gives 80%

| # | Feature | One-liner | Effort |
|---|---------|-----------|--------|
| 1 | **Session log ingest + canon extraction** | `magehand log` takes lazy bullets / Discord paste / whisper transcript; LLM pass extracts atomic facts (NPCs, promises, prices, deaths, reveals) into queryable canon with session provenance. The keystone — everything below feeds on it. | M |
| 2 | **Recap generator** | `magehand recap`: 6-line player "previously on…" + DM brief (open threads, NPC states, pending consequences). Kills the 20-min session-open amnesia and the recap homework. | S (after 1) |
| 3 | **Ruling ledger + house-rules tier** | Ingest `houserules.md` as a privileged tier; `--save` any table ruling as precedent. `ask` leads with "HOUSE/your session-9 ruling says…, RAW differs: …". Ends mid-combat re-litigation. | S |
| 4 | **NPC/place forge with auto-canonize** | `magehand npc "tiefling fence, docks"` — name/voice/want/secret wired to existing factions & open threads, no name collisions; `--save` makes it canon so next week's answers remember it. | M |
| 5 | **Thread (Chekhov) ledger** | Auto-open/close plot threads from session logs; `threads` lists open loops by staleness; prep mode suggests tonight's payoff for stale ones. Foreshadowing stops dying. | M |
| 6 | **Prep pack / module pre-flight** | `magehand prep "Chapter 4"`: one runsheet — scenes, DCs, read-alouds, every referenced stat block pulled inline from *your* PDFs (Foundry sells this per-module; your own PDFs get it free), plus stale threads and likely NPCs. | M |
| 7 | **Canon lint** | `magehand lint prep.md` — flags the duke you're about to resurrect, the tavern you renamed, the ruling you're about to contradict, with citations. Exists nowhere. | M |
| 8 | **Spoiler-safe player mode** | Visibility tags + `ask --player` (optionally a LAN/Discord endpoint): players self-serve rules/lore questions against player-visible chunks only. DM stops being a human API gateway. | S-M |
| 9 | **Table-speed adjudication modes** | `rule` (15-second speak-it-now ruling: RAW cite → closest analogy → one-liner), `stack` (condition-stack net-effect table with adv/disadv algebra), quote-first `--fast` mode for local models. | S |
| 10 | **Encounter check + monster crib** | Party table + deterministic XP-budget math in Rust + one LLM pass for qualitative mismatches ("nothing here can hit the flying PC"); `monster <name>` renders a 12-line play-priority crib from any ingested book. | M |

## Tier 2 — next wave

- **Backstory hook miner** — cross-search next session's prep against ingested
  player backstories; one tie-in per player. Cheap, high delight. (S)
- **Consequence ripple generator** — off-script event → immediate fallout +
  delayed consequences stored with triggers, resurfaced by `prep`. (M)
- **Yes-and mode** — "is there a thieves' guild here?" answered *yes, and
  consistent with retrieved canon*, contradiction-flagged, `--commit`-able. (S)
- **Who-knows-what secrets ledger** — per-fact visibility (dm/party/per-PC);
  powers spoiler-safe recaps and absent-player catch-up briefs. (M)
- **Catch-up brief** — "what Mira's character would know" for sessions 13-15,
  secrets excluded. (S, after secrets)
- **Absent-DM clerical pack**: in-world calendar/timeline (`time advance 3d`),
  party loot ledger with unidentified-item secrecy, downtime resolver that
  cites the rules it used and writes results back. (M each)
- **Source/edition conflict flag** — 2014 vs 2024 vs third-party: answer from
  your ranked primary, footnote the divergence instead of silently blending. (M)
- **DC normalizer / improv crib** — consistent DCs + fail-forward outcomes
  grounded in the DMG guidance and your party level. (S)
- **Boxed-text generator** — read-alouds that only use details actually in
  your sources, citations printed for the DM. (S)
- **Setting-consistent name well** — harvest proper nouns at ingest, mutate
  into new names, dedupe against ones already used; works offline. (S)
- **Spotlight tracker** — per-player scene-time from session logs; nudges when
  a shy player's threads go stale. (M)
- **Stat block extract/normalize + stub drafting** from comparables. (M)
- **Onboarding packet** — one-page "what the table knows" primer for a new
  player, generated from canon. (S)

## Deliberately not building (solved or wrong shape)

Dice rollers, character builders, battle maps / VTT features, initiative
trackers (Improved Initiative et al. are fine — though the combat scratchpad
with inline rule citations is a maybe), generic random generators (donjon),
AI art, voice-acted AI DMs. The seam magehand owns: *your* books + *your*
campaign, local, citable, written back.

## Suggested build order

1 → 2 → 3 (each S/M, immediate weekly payoff) → 8 → 4 → 5 → 6 → 7, then
tier 2 by taste. Item 1's ingest tags and write-back are the two primitives —
design those first; everything else is prompt modes and small tables on top.
