mod campaign;
mod improv;
mod ledger;
mod listen;
mod players;
mod serve;
mod signals;

use rusqlite::Connection;
use serde_json::{json, Value};
use std::error::Error;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

pub(crate) type Result<T> = std::result::Result<T, Box<dyn Error>>;

const DB_PATH: &str = "magehand.db";
pub(crate) const SOURCES_DIR: &str = "sources";
const TOP_K: usize = 8;
const MAX_CHUNK: usize = 2000;

// TODO(you): tune your table's style here — strict rules-as-written vs
// improv-friendly, homebrew policy, spoiler handling for published adventures.
const SYSTEM_PROMPT: &str = "You are Magehand, a D&D 5e rules expert assisting a Dungeon Master \
mid-game. Answer from the provided excerpts and name the section each ruling comes from. \
Excerpts are labeled [HOUSE RULE], [CAMPAIGN NOTE], or [RULEBOOK]. Precedence: house rules and \
recorded table rulings override rulebook text; campaign notes are established canon — when sources \
conflict, lead with the higher-precedence one and flag the conflict explicitly. If two RULEBOOK \
excerpts disagree (different editions or publishers), present both and name their sources. \
If the excerpts don't settle the question, say so, then give a sensible ruling clearly marked as \
your own judgment. Lead with the ruling, then brief reasoning — the table is waiting.";

const PLAYER_PROMPT: &str = "You are Magehand, a rules assistant for a PLAYER at a tabletop RPG \
table. Answer from the provided excerpts (rulebooks and the table's house rules) and cite the \
section for each answer. House rules override book text — flag when they differ. Never speculate \
about the DM's adventure content, monster secrets, or anything not in the excerpts; if a question \
needs DM knowledge, say it's the DM's call. Be brief.";

fn main() {
    // `magehand search ... | head` must not panic mid-pipe.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let player = args.iter().any(|a| a == "--player");
    if player {
        // On other commands the flag would be silently ignored (or worse, swallowed
        // out of logged text) — a DM must not believe `recap --player` was safe.
        match args.first().map(String::as_str) {
            Some("search" | "ask" | "chat") => args.retain(|a| a != "--player"),
            _ => {
                eprintln!("error: --player only applies to search/ask/chat");
                std::process::exit(2);
            }
        }
    }
    let rest = |i: usize| args[i..].join(" ");
    let result = match args.first().map(String::as_str) {
        Some("ingest") => ingest(),
        Some("search") if args.len() > 1 => cmd_search(&rest(1), player),
        Some("ask") if args.len() > 1 => cmd_ask(&rest(1), player),
        Some("chat") => cmd_chat(player),
        Some("log") if args.len() > 1 => campaign::cmd_log(&args[1..]),
        Some("recap") => campaign::cmd_recap(),
        Some("ruling") if args.len() > 1 => campaign::cmd_ruling(&rest(1)),
        Some("npc") if args.len() > 1 => campaign::cmd_npc(&args[1..]),
        Some("thread") => campaign::cmd_thread(&args[1..]),
        Some("prep") if args.len() > 1 => campaign::cmd_prep(&rest(1)),
        Some("lint") if args.len() == 2 => campaign::cmd_lint(&args[1]),
        Some("yesand") if args.len() > 1 => improv::cmd_yesand(&args[1..]),
        Some("consequence") if args.len() > 1 => improv::cmd_consequence(&args[1..]),
        Some("boxtext") if args.len() > 1 => improv::cmd_boxtext(&rest(1)),
        Some("name") if args.len() > 1 => improv::cmd_name(&args[1..]),
        Some("hooks") if args.len() > 1 => players::cmd_hooks(&rest(1)),
        Some("secret") => players::cmd_secret(&args[1..]),
        Some("catchup") if args.len() > 1 => players::cmd_catchup(&args[1..]),
        Some("onboard") => players::cmd_onboard(),
        Some("spotlight") => players::cmd_spotlight(),
        Some("time") => ledger::cmd_time(&args[1..]),
        Some("timeline") => ledger::cmd_timeline(),
        Some("loot") => ledger::cmd_loot(&args[1..]),
        Some("downtime") if args.len() > 1 => ledger::cmd_downtime(&args[1..]),
        Some("statblock") if args.len() > 1 => improv::cmd_statblock(&args[1..]),
        Some("listen") => listen::cmd_listen(&args[1..]),
        Some("cards") => signals::cmd_cards(&args[1..]),
        Some("serve") => serve::cmd_serve(&args[1..]),
        _ => {
            eprintln!("usage: magehand <command> [--player]");
            eprintln!("  ingest                       index everything under sources/");
            eprintln!("  search <terms>               raw retrieval hits");
            eprintln!("  ask <question>               one-shot RAG answer");
            eprintln!("  chat                         interactive session");
            eprintln!("  log <file|-|text>            archive session notes into campaign canon");
            eprintln!("  recap                        player recap + DM brief from recent sessions");
            eprintln!("  ruling <text>                record a table ruling (overrides RAW in answers)");
            eprintln!("  npc <description> [--save]   canon-aware NPC; --save writes it into the vault");
            eprintln!("  thread add <title> | list | close <slug>");
            eprintln!("  prep <chapter or topic>      one-page session runsheet");
            eprintln!("  lint <file>                  check draft notes against established canon");
            eprintln!("  yesand <question> [--commit] say yes without breaking canon; --commit records it");
            eprintln!("  consequence <event> [--save] fallout now + delayed consequences as threads");
            eprintln!("  boxtext <scene>              read-aloud text from your sources only");
            eprintln!("  name <culture/kind>          setting-consistent names, deduped vs your NPCs");
            eprintln!("  hooks <next session topic>   one backstory tie-in per character");
            eprintln!("  secret add <player> <text> | list [player]");
            eprintln!("  catchup <player> [missed]    player-safe brief for absent players");
            eprintln!("  onboard                      one-page primer for a new player");
            eprintln!("  spotlight                    who's been quiet + a scene for each");
            eprintln!("  time [advance <d> | set <d>] in-world calendar (announces due threads)");
            eprintln!("  timeline                     campaign chronology from session records");
            eprintln!("  loot <list | add | fund>     party loot & fund ledger");
            eprintln!("  downtime <activity> [--commit] resolve by the book; commit charges fund + days");
            eprintln!("  statblock <name|--stub desc> [--save] play crib or homebrew draft");
            eprintln!("  listen [--stdin] [--shadow]  live table transcript + signal cards; Ctrl-C archives");
            eprintln!("  cards [date]                 review a session's card log (grade shadow runs)");
            eprintln!("  serve [--port N]             DM dashboard: live card feed + one-tap actions (LAN)");
            eprintln!("  --player                     spoiler-safe retrieval (search/ask/chat)");
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        let msg = e.to_string();
        eprintln!("error: {msg}");
        if msg.contains("no such column") {
            eprintln!("hint: the index schema changed — re-run `magehand ingest`");
        }
        std::process::exit(1);
    }
}

// ---------- storage ----------

pub(crate) fn open_db() -> Result<Connection> {
    let conn = Connection::open(DB_PATH)?;
    // serve (tap → ruling → reindex) and listen (queries) run as separate
    // processes on the same db — wait on a lock rather than erroring
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS chunks
         USING fts5(source, title, body, tier UNINDEXED, vis UNINDEXED)",
    )?;
    Ok(conn)
}

pub(crate) struct Hit {
    pub(crate) source: String,
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) tier: String,
    pub(crate) score: f64,
}

/// FTS5 MATCH syntax chokes on raw questions; keep only quoted terms OR'd together.
pub(crate) fn fts_query(q: &str) -> String {
    q.split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 1)
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(" OR ")
}

pub(crate) fn search(
    conn: &Connection,
    query: &str,
    k: usize,
    tier: Option<&str>,
    player_only: bool,
) -> Result<Vec<Hit>> {
    let q = fts_query(query);
    if q.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT source, title, body, tier, bm25(chunks, 0.5, 4.0, 1.0, 0.0, 0.0) AS score
         FROM chunks WHERE chunks MATCH ?1
           AND (?2 IS NULL OR tier = ?2)
           AND (?3 = 0 OR vis = 'player')
         ORDER BY score LIMIT ?4",
    )?;
    let rows = stmt.query_map(rusqlite::params![q, tier, player_only as i64, k as i64], |r| {
        Ok(Hit {
            source: r.get(0)?,
            title: r.get(1)?,
            body: r.get(2)?,
            tier: r.get(3)?,
            score: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

pub(crate) fn context_block(hits: &[Hit]) -> String {
    hits.iter()
        .map(|h| format!("### [{}] {} — {}\n{}", tier_label(&h.tier), h.source, h.title, h.body))
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn tier_label(tier: &str) -> &'static str {
    match tier {
        "house" => "HOUSE RULE",
        "campaign" => "CAMPAIGN NOTE",
        _ => "RULEBOOK",
    }
}

// ---------- ingest ----------

pub(crate) fn ingest() -> Result<()> {
    if !Path::new(SOURCES_DIR).is_dir() {
        return Err(format!("no `{SOURCES_DIR}/` directory — put rulebooks (md/txt/json/pdf) there first").into());
    }
    let mut conn = open_db()?;
    let mut files = Vec::new();
    collect_files(Path::new(SOURCES_DIR), &mut files)?;
    files.sort();

    let tx = conn.transaction()?;
    // full rebuild: corpus is small; drop+create keeps ingest idempotent and migrates old schemas
    tx.execute_batch(
        "DROP TABLE IF EXISTS chunks;
         CREATE VIRTUAL TABLE chunks USING fts5(source, title, body, tier UNINDEXED, vis UNINDEXED);",
    )?;
    let (mut n_files, mut n_chunks) = (0usize, 0usize);
    for path in &files {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
        let source = path.strip_prefix(SOURCES_DIR).unwrap_or(path).to_string_lossy().to_string();
        let chunks = match ext.as_str() {
            "md" | "markdown" | "txt" => chunk_markdown(&read_lossy(path)?, &source),
            "json" => chunk_json(&read_lossy(path)?, &source),
            "pdf" => match pdf_extract::extract_text(path) {
                Ok(text) => chunk_plain(&text, &source),
                Err(e) => {
                    eprintln!("skipping {source}: {e}");
                    continue;
                }
            },
            _ => continue,
        };
        let (tier, vis) = classify(&source);
        let mut stmt = tx.prepare_cached(
            "INSERT INTO chunks (source, title, body, tier, vis) VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for (title, body) in &chunks {
            stmt.execute(rusqlite::params![source, title, body, tier, vis])?;
        }
        n_files += 1;
        n_chunks += chunks.len();
    }
    drop(files);
    tx.commit()?;
    println!("indexed {n_chunks} chunks from {n_files} files");
    Ok(())
}

/// Tier decides answer precedence; vis gates --player retrieval.
/// sources/house/**            -> house rules & table rulings, player-visible
/// sources/campaign*/**        -> campaign canon, DM-only (recaps/ and public/ are player-visible)
/// sources/dm/**, sources/dm-* -> spoiler-heavy books (adventures), DM-only
/// files at sources/ root      -> DM-only (strays fail closed — this is a spoiler boundary)
/// everything else             -> rulebooks, player-visible
/// Matching is case-insensitive so `Campaign/` on a case-insensitive FS can't fail open.
fn classify(source: &str) -> (&'static str, &'static str) {
    let s = source.replace('\\', "/").to_lowercase();
    let Some((seg, rest)) = s.split_once('/') else {
        return ("rules", "dm");
    };
    if seg == "house" {
        ("house", "player")
    } else if seg.starts_with("campaign") {
        if rest.starts_with("recaps/") || rest.starts_with("public/") {
            ("campaign", "player")
        } else {
            ("campaign", "dm")
        }
    } else if seg == "dm" || seg.starts_with("dm-") {
        ("rules", "dm")
    } else {
        ("rules", "player")
    }
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue; // .git and friends
        }
        let path = entry.path();
        let is_symlink = entry.file_type()?.is_symlink();
        if path.is_dir() {
            if is_symlink {
                continue; // symlinked dirs invite loops and double-indexing; symlinked files are fine
            }
            collect_files(&path, out)?;
        } else {
            // raw ASR transcripts (sessions/<date>-live*.md) stay out of the index —
            // scoped to sessions/ so a note like npcs/where-they-live.md still indexes
            if dir.file_name().is_some_and(|d| d == "sessions")
                && path.file_stem().and_then(|s| s.to_str()).is_some_and(|s| s.contains("-live"))
            {
                continue;
            }
            out.push(path);
        }
    }
    Ok(())
}

/// One chunk per heading section, oversized sections split on paragraphs.
fn chunk_markdown(text: &str, source: &str) -> Vec<(String, String)> {
    let text = strip_frontmatter(text);
    let mut chunks = Vec::new();
    let mut title = stem(source);
    let mut body = String::new();
    let mut in_fence = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        if !in_fence && line.starts_with('#') {
            flush_section(&mut chunks, &title, &mut body);
            title = line.trim_start_matches('#').trim().to_string();
        } else {
            body.push_str(line);
            body.push('\n');
        }
    }
    flush_section(&mut chunks, &title, &mut body);
    chunks
}

/// YAML frontmatter is vault metadata, not searchable prose. Line-anchored and
/// CRLF-tolerant; a leading horizontal rule or an unclosed block stays indexed.
pub(crate) fn strip_frontmatter(text: &str) -> &str {
    let mut lines = text.split_inclusive('\n');
    let Some(first) = lines.next() else { return text };
    if first.trim_end() != "---" {
        return text;
    }
    let mut consumed = first.len();
    for (i, line) in lines.enumerate() {
        consumed += line.len();
        if line.trim_end() == "---" {
            return &text[consumed..];
        }
        // frontmatter is a short run of `key: value` lines; anything else means
        // that leading --- was a horizontal rule, not an opener
        if i >= 40 || (i == 0 && !line.contains(':')) {
            return text;
        }
    }
    text
}

fn flush_section(chunks: &mut Vec<(String, String)>, title: &str, body: &mut String) {
    if !body.trim().is_empty() {
        for piece in split_max(body.trim(), MAX_CHUNK) {
            chunks.push((title.to_string(), piece));
        }
    }
    body.clear();
}

fn chunk_plain(text: &str, source: &str) -> Vec<(String, String)> {
    split_max(text, MAX_CHUNK)
        .into_iter()
        .enumerate()
        .map(|(i, piece)| (format!("{} (part {})", stem(source), i + 1), piece))
        .collect()
}

/// JSON arrays (e.g. 5e-database exports) become one chunk per entity.
fn chunk_json(text: &str, source: &str) -> Vec<(String, String)> {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return chunk_plain(text, source);
    };
    let items: Vec<&Value> = match &value {
        Value::Array(arr) => arr.iter().collect(),
        other => vec![other],
    };
    let mut chunks = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let title = item
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("{} #{}", stem(source), i + 1));
        let mut body = String::new();
        flatten_json(item, &mut body);
        for piece in split_max(body.trim(), MAX_CHUNK) {
            chunks.push((title.clone(), piece));
        }
    }
    chunks
}

fn flatten_json(v: &Value, out: &mut String) {
    match v {
        Value::Object(map) => {
            for (k, val) in map {
                match val {
                    Value::Object(_) | Value::Array(_) => {
                        out.push_str(k);
                        out.push_str(":\n");
                        flatten_json(val, out);
                    }
                    _ => {
                        out.push_str(k);
                        out.push_str(": ");
                        push_scalar(val, out);
                    }
                }
            }
        }
        Value::Array(arr) => {
            for val in arr {
                flatten_json(val, out);
            }
        }
        other => push_scalar(other, out),
    }
}

fn push_scalar(v: &Value, out: &mut String) {
    match v {
        Value::String(s) => out.push_str(s),
        other => out.push_str(&other.to_string()),
    }
    out.push('\n');
}

/// Pack paragraphs into ≤ max-byte chunks; a lone oversized paragraph gets hard-split.
fn split_max(text: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for para in text.split("\n\n") {
        if para.trim().is_empty() {
            continue;
        }
        if !cur.is_empty() && cur.len() + para.len() > max {
            out.push(std::mem::take(&mut cur).trim().to_string());
        }
        if para.len() > max {
            out.extend(hard_split(para, max)); // ponytail: char-boundary split, good enough for pathological PDFs
        } else {
            cur.push_str(para);
            cur.push_str("\n\n");
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

fn hard_split(s: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in s.chars() {
        cur.push(ch);
        if cur.len() >= max {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// One mis-encoded rulebook shouldn't abort the whole ingest.
pub(crate) fn read_lossy(path: &Path) -> Result<String> {
    Ok(String::from_utf8_lossy(&std::fs::read(path)?).into_owned())
}

fn stem(source: &str) -> String {
    Path::new(source)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| source.to_string())
}

// ---------- LLM (OpenAI-compatible: OpenRouter or Ollama) ----------

pub(crate) struct Llm {
    base: String,
    key: String,
    model: String,
}

impl Llm {
    /// Whether prompts sent here stay on this machine (the listener's
    /// secret-detection gate hangs off this — so it parses the host, never a
    /// substring: `localhost.evil.com` and `127.0.0.1.nip.io` are NOT local).
    pub(crate) fn is_local(&self) -> bool {
        let after_scheme = self.base.rsplit("://").next().unwrap_or(&self.base);
        let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
        let host = authority.rsplit('@').next().unwrap_or(authority);
        // strip :port (but keep bracketed IPv6 intact)
        let host = if host.starts_with('[') {
            host.split(']').next().unwrap_or(host).trim_start_matches('[')
        } else {
            host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host)
        };
        host == "localhost"
            || host == "::1"
            || host.strip_prefix("127.").is_some_and(|r| r.split('.').count() == 3)
    }
}

/// llm_config with a per-role model override (e.g. a cheap classifier model
/// for the live listener via MAGEHAND_LISTEN_MODEL).
pub(crate) fn llm_config_for(model_env: &str) -> Llm {
    let mut llm = llm_config();
    if let Ok(m) = std::env::var(model_env) {
        if !m.is_empty() {
            llm.model = m;
        }
    }
    llm
}

pub(crate) fn llm_config() -> Llm {
    let or_key = std::env::var("OPENROUTER_API_KEY").ok().filter(|k| !k.is_empty());
    let base = std::env::var("MAGEHAND_BASE_URL").unwrap_or_else(|_| {
        if or_key.is_some() {
            "https://openrouter.ai/api/v1".into()
        } else {
            "http://localhost:11434/v1".into() // Ollama
        }
    });
    // Defaults follow the base actually in use, so a stale OPENROUTER_API_KEY
    // can't pick an OpenRouter model (or leak the key) against a local endpoint.
    let is_openrouter = base.contains("openrouter.ai");
    let key = std::env::var("MAGEHAND_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
        .or(if is_openrouter { or_key } else { None })
        .unwrap_or_else(|| "ollama".into());
    let model = std::env::var("MAGEHAND_MODEL").unwrap_or_else(|_| {
        if is_openrouter {
            "openrouter/auto".into()
        } else {
            "llama3.1".into()
        }
    });
    Llm { base, key, model }
}

pub(crate) fn chat_completion(llm: &Llm, messages: &[Value]) -> Result<String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(120)) // a stalled server must not hang the table
        .build();
    let resp = agent
        .post(&format!("{}/chat/completions", llm.base))
        .set("Authorization", &format!("Bearer {}", llm.key))
        .send_json(json!({ "model": llm.model, "messages": messages }));
    let body: Value = match resp {
        Ok(r) => r.into_json()?,
        Err(ureq::Error::Status(code, r)) => {
            let text = r.into_string().unwrap_or_default();
            return Err(format!("LLM API error {code}: {text}").into());
        }
        Err(e) => return Err(e.into()),
    };
    body["choices"][0]["message"]["content"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("unexpected LLM response: {body}").into())
}

// ---------- RAG ----------

/// One cheap LLM call buys paraphrase recall without a vector index.
/// ponytail: swap for sqlite-vec + embeddings if this ever feels thin.
fn expand_query(llm: &Llm, question: &str) -> String {
    let prompt = format!(
        "List the D&D 5e rulebook terms this question is about, as 3-8 comma-separated \
         keywords (rule names, conditions, spell names). Only keywords, no prose.\nQuestion: {question}"
    );
    match chat_completion(llm, &[json!({"role": "user", "content": prompt})]) {
        Ok(keywords) => format!("{question} {keywords}"),
        Err(_) => question.to_string(), // retrieval still works unexpanded
    }
}

fn answer(
    conn: &Connection,
    llm: &Llm,
    history: &mut Vec<Value>,
    question: &str,
    player: bool,
) -> Result<String> {
    let hits = search(conn, &expand_query(llm, question), TOP_K, None, player)?;
    let context = context_block(&hits);
    let user_msg = if context.is_empty() {
        format!("(no rulebook excerpts matched — run `magehand ingest`?)\n\nQuestion: {question}")
    } else {
        format!("Source excerpts:\n\n{context}\n\nQuestion: {question}")
    };

    let system = if player { PLAYER_PROMPT } else { SYSTEM_PROMPT };
    let mut messages = vec![json!({"role": "system", "content": system})];
    messages.extend(history.iter().cloned());
    messages.push(json!({"role": "user", "content": user_msg}));
    let reply = chat_completion(llm, &messages)?;

    // History keeps the bare question — excerpts would balloon every later turn.
    history.push(json!({"role": "user", "content": question}));
    history.push(json!({"role": "assistant", "content": reply}));
    Ok(reply)
}

// ---------- commands ----------

fn cmd_search(query: &str, player: bool) -> Result<()> {
    if fts_query(query).is_empty() {
        println!("query has no searchable terms (words of 2+ letters/digits)");
        return Ok(());
    }
    let conn = open_db()?;
    let hits = search(&conn, query, TOP_K, None, player)?;
    if hits.is_empty() {
        println!("no matches (did you run `magehand ingest`?)");
    }
    for h in hits {
        let preview: String = h.body.chars().take(200).collect();
        println!("[{:.2}] [{}] {} — {}\n{}\n", h.score, tier_label(&h.tier), h.source, h.title, preview);
    }
    Ok(())
}

fn cmd_ask(question: &str, player: bool) -> Result<()> {
    let conn = open_db()?;
    let llm = llm_config();
    println!("{}", answer(&conn, &llm, &mut Vec::new(), question, player)?);
    Ok(())
}

fn cmd_chat(player: bool) -> Result<()> {
    let conn = open_db()?;
    let llm = llm_config();
    let mut history = Vec::new();
    let mode = if player { " [player mode]" } else { "" };
    println!("Magehand{mode} — {} via {}", llm.model, llm.base);
    println!("Ask rules questions. `/search <terms>` shows raw retrieval. Ctrl-D quits.\n");
    let stdin = std::io::stdin();
    loop {
        print!("🖐  ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            break; // EOF
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("/search") {
            if rest.is_empty() || rest.starts_with(char::is_whitespace) {
                let q = rest.trim();
                if q.is_empty() {
                    println!("usage: /search <terms>");
                } else {
                    cmd_search(q, player)?;
                }
                continue;
            }
        }
        match answer(&conn, &llm, &mut history, line, player) {
            Ok(reply) => println!("\n{reply}\n"),
            Err(e) => eprintln!("error: {e}"),
        }
    }
    Ok(())
}
