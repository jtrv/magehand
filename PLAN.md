# PLAN

Online-play webapp pivot. Design of record: `/home/sugimoto/.config/claude/plans/reactive-discovering-shell.md`
(M0 hosting shell, M1 whispers, M2 sheets/party, M3 map — DONE and verified in-session, uncommitted.
M4 voice+transcription — server routes wired, functions missing, tree does not compile).

## Execution queue

Verify command: `RUSTC_WRAPPER="" cargo clippy --all-targets -- -D warnings && RUSTC_WRAPPER="" cargo build`

- [x] 1. Finish M4 server + voice client core so the tree compiles: create `src/voice.js`
      (WebRTC full-mesh + AudioWorklet 16 kHz silence-gated WAV capture, spec in PLAN notes
      below) and implement the missing serve.rs functions (`rtc_config`, `rtc_relay`,
      `post_audio`, `session_ctl` with start/end lifecycle, `transcribe` multipart POST to
      whisper-server) plus `rtc`/`session` SSE events on the DM stream. Acceptance: verify
      command passes. Then commit ALL work to date as two commits:
      `feat: multi-campaign hosting, whispers, tap sheets, shared map` and
      `feat: voice mesh + per-player transcription (server + client core)`.
- [x] 2. M4 UI wiring: dm.html gains Join voice / Start session / End session buttons +
      peer pills with per-peer mute; player.html gains a voice bar (Join voice, self-mute,
      peer count). Both load `voice.js` relative. Acceptance: verify passes + serve boots
      and both pages render the new controls (curl for element ids). Commit.
- [x] 3. E2E transcription fixture: script in scratchpad spins a fake whisper-server
      (python http server returning canned JSON), starts a session via POST /session,
      POSTs two WAVs as two different identities (player cookie + DM), asserts the live
      transcript gains `- [ts] Mira: …` and `- [ts] DM: …` lines and a cards jsonl line
      appears for a lexicon hit. Acceptance: script exits 0 with those asserts. No commit
      of scratch files; commit any product fixes it forced.
- [x] 4. Codex adversarial review of `git diff HEAD~2` (the two feature commits): dispatch
      the codex rescue agent to hunt bugs/races/security holes in serve.rs + voice.js;
      triage its findings, fix the confirmed ones. Acceptance: verify passes; findings and
      dispositions listed in the report. Commit fixes as `fix: review findings`.
- [x] 5. Docs + cleanup: README "Online play" section (host/caddy/coturn/whisper-server
      setup, env vars MAGEHAND_STT_URL / MAGEHAND_TURN_*, consent posture: audio
      transcribed on host, WAVs deleted after transcription); TABLE-MODE.md addendum
      noting which in-person refusals flip online and why; commit the user's pending
      README screenshot hunks + docs/*.webp with it; delete stray test PNGs and
      .playwright-mcp/ from repo root and gitignore them. Acceptance: verify passes,
      `git status` clean afterward. Commit as `docs: online play`.

### Notes for task 1 (spec)

voice.js exposes `window.Voice = { join(id, eventSource, onPeers), leave(), selfMute(on), mute(peerId, on) }`.
Mesh: on join POST `rtc {to:'*', type:'join'}`; existing peers initiate the offer to the
newcomer (newcomer only answers → no glare except double-join; on offer-while-have-local-offer,
larger id rolls back via `setLocalDescription({type:'rollback'})`). Config from GET `rtc-config`.
Remote audio into hidden autoplay `<audio>` elements keyed `aud-<peer>`. Capture: AudioWorklet
(inline Blob module) → RMS gate (~0.012, 700 ms hangover, 400 ms min, 15 s cap) → downsample
context rate→16 kHz mono → PCM16 WAV (44-byte header) → POST `audio` (relative paths work on
both pages). Server: `transcribe()` = hand-built multipart (file + response_format=json +
prompt=hotwords) via ureq to `MAGEHAND_STT_URL` (default `http://127.0.0.1:9090/inference`),
60 s timeout. Session start: `open_live` + `signals::Listener::new(false)` fed by
sync_channel (listen.rs:52-60 pattern) + single STT worker consuming a Job channel, writing
`- [ts] Name: text` via `append_line` (speaker prefix inside text). Session end: take
`State.live`, drop jobs sender, join handles, `listen::finalize(&path, &build_lexicon(), true)`.
rtc_relay stamps `from` server-side; `to:'*'` fans to all rostered slugs ≠ sender (+ DM if
sender isn't DM); collect slugs BEFORE locking State (lock-order). RUSTC_WRAPPER="" for all
cargo invocations on this machine.

### Log
- 2026-08-18 T1 feat ×2 (700d6ba hosting shell, a9624ee online play core) — verify clean first run
- 2026-08-18 T2 feat (f917151 voice/session controls both pages) — verify clean
- 2026-08-18 T3 fix (ff50e33 entity cards emitted from Listener::push_line, both paths) — e2e script ALL PASS ×2
- 2026-08-18 T5 docs (5710012 README online play, TABLE-MODE addendum, gitignore/cleanup) — verify clean
- 2026-08-18 T4 fix (aa6d134 nine codex-review hardening fixes) — 10 findings: 9 fixed, 1 reshaped (silent transcription failure → throttled DM card instead of retry queue); fixture ALL PASS
