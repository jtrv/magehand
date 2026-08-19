use crate::campaign::{cmd_ruling, cmd_thread, ensure_vault, md_files, CAMPAIGN};
use crate::ledger::{current_day, fm_value};
use crate::sheets;
use crate::{answer, llm_config, open_db, read_lossy, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tiny_http::{Header, Method, Response, Server};

const CARDS_DIR: &str = ".magehand/live";
const DM_HTML: &str = include_str!("dm.html");
const PLAYER_HTML: &str = include_str!("player.html");
const VOICE_JS: &str = include_str!("voice.js");
const POLL: Duration = Duration::from_millis(700);
const MAX_BODY: usize = 16 * 1024;
const ASK_PER_HOUR: usize = 12;
const ASK_PER_DAY: usize = 150; // per campaign — hosted means the host pays the table's LLM bill
const MAX_IMAGE: usize = 8 * 1024 * 1024;
const MAX_WAV: usize = 2 * 1024 * 1024; // ~60s of 16kHz PCM16 — one gated utterance
const PIN_TRIES_PER_HOUR: usize = 5;
const MSG_PER_HOUR: usize = 60;
const RTC_BACKLOG: usize = 4096; // per-peer signaling queue cap — an absent reader must not grow memory forever
const SSE_PER_PLAYER: usize = 4;
const SSE_DM: usize = 8;
const TOKENS_PATH: &str = ".magehand/tokens.json";

/// Player roster + capability tokens (mutable: the DM can regenerate a leaked
/// link), per-player ask rate limiting, a campaign-wide daily ask cap, and
/// PIN-attempt throttling.
struct Players {
    by_token: Mutex<HashMap<String, PInfo>>,
    dm_token: String,
    ask_log: Mutex<HashMap<String, Vec<Instant>>>,
    campaign_asks: Mutex<Vec<Instant>>,
    pin_log: Mutex<HashMap<String, Vec<Instant>>>,
    msg_log: Mutex<HashMap<String, Vec<Instant>>>,
}

/// Live SSE stream counts, keyed by player slug ("\0dm" for the DM feed —
/// NUL can't appear in a slug). A phone that reconnects without closing old
/// streams must not pile up polling threads forever.
static SSE_COUNTS: std::sync::LazyLock<Mutex<HashMap<String, usize>>> =
    std::sync::LazyLock::new(Mutex::default);

/// One reserved stream slot; Drop returns it, so every exit path of a stream
/// thread (client gone, write error) releases the slot.
struct SseSlot(String);

impl SseSlot {
    fn take(key: &str, cap: usize) -> Option<Self> {
        let mut counts = SSE_COUNTS.lock().unwrap();
        let n = counts.entry(key.to_string()).or_insert(0);
        if *n >= cap {
            return None;
        }
        *n += 1;
        Some(Self(key.to_string()))
    }
}

impl Drop for SseSlot {
    fn drop(&mut self) {
        let mut counts = SSE_COUNTS.lock().unwrap();
        if let Some(n) = counts.get_mut(&self.0) {
            *n = n.saturating_sub(1);
        }
    }
}

#[derive(Clone)]
struct PInfo {
    slug: String,
    name: String,
    token: String,
    /// Empty until the player's first claim. Stored plaintext: the threat model
    /// is a leaked join link, not the DM (who owns the vault file anyway) —
    /// the real defense is PIN_TRIES_PER_HOUR.
    pin: String,
}

/// Where this server is reachable from a browser. Standalone LAN serve: the
/// LAN URL with an empty path prefix. Behind `magehand host` + caddy: the
/// public https URL with a `/c/<slug>` prefix (caddy strips it before us, so
/// the prefix only appears in cookies, redirects, and join links).
struct Base {
    url: String,
    path: String,
    secure: bool,
}

/// Phase 3 of table mode: the DM dashboard. A LAN web page (token-gated, since
/// it shows secrets) that tails the listener's card JSONL over SSE and turns
/// one tap into an existing vault command. Decoupled from `listen` by design —
/// the cards file is the bus, so a dashboard crash never touches transcription.
pub(crate) fn cmd_serve(args: &[String]) -> Result<()> {
    ensure_vault()?;
    let port: u16 = arg_val(args, "--port").and_then(|v| v.parse().ok()).unwrap_or(7979);
    let server = Server::http(("0.0.0.0", port))
        .map_err(|e| format!("couldn't bind port {port}: {e}"))?;

    let base = Arc::new(build_base(args, port));
    let cards_path = format!("{CARDS_DIR}/cards-{}.jsonl", crate::campaign::today());
    let processed = read_lossy(Path::new(&cards_path))
        .map(|text| text.lines().filter(|line| !line.trim().is_empty()).count())
        .unwrap_or(0);
    let state = Arc::new(Mutex::new(State {
        processed,
        map_json: map_state().to_string(),
        ..State::default()
    }));
    spawn_poller(Arc::clone(&state));
    let (players, token) = build_players()?;
    let players = Arc::new(players);

    println!("magehand table server\n");
    println!("  DM dashboard (shows secrets):  {}/?t={token}", base.url);
    println!("  player join page (QR codes):   {}/join?t={token}", base.url);
    println!(
        "\n{} player(s) rostered. Open the join page on the DM laptop and let players scan.",
        players.by_token.lock().unwrap().len()
    );
    println!("run `magehand listen` alongside this to feed the card feed. Ctrl-C to stop.");

    for request in server.incoming_requests() {
        let method = request.method().clone();
        let url = request.url().to_string();
        let path = url.split('?').next().unwrap_or("/").to_string();

        // --- player routes (own capability cookie, scoped to one player) ---
        if let Some(seg) = path.strip_prefix("/p/") {
            route_player(request, &method, seg, &players, &state, &base);
            continue;
        }

        // --- DM routes (require the DM token) ---
        if !authed(&request, &token) {
            if method == Method::Get
                && (path == "/" || path == "/join")
                && query_token(&url).as_deref() == Some(&token)
            {
                // set the cookie, then bounce to a clean URL so the token stays
                // out of history, screenshots, and proxy logs
                let _ = request.respond(
                    redirect(&format!("{}{path}", base.path))
                        .with_header(cookie_hdr("mh", &token, &base)),
                );
            } else {
                let _ = request.respond(Response::from_string("unauthorized").with_status_code(401));
            }
            continue;
        }

        match (&method, path.as_str()) {
            (Method::Get, "/") => {
                let _ = request.respond(Response::from_string(DM_HTML).with_header(html_hdr()).with_header(nostore_hdr()));
            }
            (Method::Get, "/join") => {
                let _ = request.respond(join_page(&players, &base.url).with_header(html_hdr()).with_header(nostore_hdr()));
            }
            (Method::Get, "/events") => match SseSlot::take("\0dm", SSE_DM) {
                Some(slot) => {
                    let st = Arc::clone(&state);
                    std::thread::spawn(move || {
                        let _slot = slot;
                        stream_events(request, st);
                    });
                }
                None => {
                    let _ = request
                        .respond(Response::from_string("too many streams").with_status_code(429));
                }
            },
            (Method::Post, "/action") => {
                // off the accept loop: a slow/held-open body must not freeze the
                // whole dashboard for every other client
                let st = Arc::clone(&state);
                std::thread::spawn(move || handle_action(request, st));
            }
            (Method::Post, "/regen") => {
                regen_link(request, &players, &token, &base);
            }
            (Method::Get, "/voice.js") => {
                let _ = request.respond(
                    Response::from_string(VOICE_JS)
                        .with_header(header("Content-Type", "application/javascript"))
                        .with_header(nostore_hdr()),
                );
            }
            (Method::Get, "/rtc-config") => respond_json(request, rtc_config()),
            (Method::Post, "/rtc") => {
                let st = Arc::clone(&state);
                let pl = Arc::clone(&players);
                std::thread::spawn(move || rtc_relay(request, "dm", &pl, &st));
            }
            (Method::Post, "/audio") => {
                let st = Arc::clone(&state);
                std::thread::spawn(move || post_audio(request, "DM".into(), &st));
            }
            (Method::Post, "/session") => {
                let st = Arc::clone(&state);
                std::thread::spawn(move || session_ctl(request, &st));
            }
            (Method::Get, "/map/image") => serve_map_image(request),
            (Method::Post, "/map/image") => {
                let st = Arc::clone(&state);
                std::thread::spawn(move || upload_map_image(request, &st));
            }
            (Method::Post, "/map/token") => {
                let st = Arc::clone(&state);
                std::thread::spawn(move || dm_map_token(request, &st));
            }
            _ => {
                let _ = request.respond(Response::from_string("not found").with_status_code(404));
            }
        }
    }
    Ok(())
}

fn build_base(args: &[String], port: u16) -> Base {
    match arg_val(args, "--public-base") {
        Some(url) => {
            let url = url.trim_end_matches('/').to_string();
            let path = url
                .split_once("://")
                .and_then(|(_, rest)| rest.find('/').map(|i| rest[i..].to_string()))
                .unwrap_or_default();
            let secure = url.starts_with("https://");
            Base { url, path, secure }
        }
        None => Base { url: lan_url(port), path: String::new(), secure: false },
    }
}

fn redirect(location: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string("").with_status_code(303).with_header(header("Location", location))
}

// ---------- token store ----------

/// Tokens persist across restarts (`.magehand/tokens.json`): hosted join links
/// live in players' phones for a whole campaign, so a restart must not orphan
/// them. New roster members get minted in; departed slugs are dropped.
fn build_players() -> Result<(Players, String)> {
    let saved = read_lossy(Path::new(TOKENS_PATH))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .unwrap_or(Value::Null);
    let dm_token = match saved["dm"].as_str() {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => mint_token()?,
    };
    let mut by_token = HashMap::new();
    for slug in sheets::roster() {
        let (token, pin) = match saved["players"][&slug].as_object() {
            Some(o) => (
                o.get("token").and_then(Value::as_str).unwrap_or_default().to_string(),
                o.get("pin").and_then(Value::as_str).unwrap_or_default().to_string(),
            ),
            None => (String::new(), String::new()),
        };
        let token = if token.is_empty() { mint_token()? } else { token };
        by_token.insert(
            token.clone(),
            PInfo { slug: slug.clone(), name: sheets::display_name(&slug), token, pin },
        );
    }
    save_tokens(&dm_token, &by_token)?;
    let players = Players {
        by_token: Mutex::new(by_token),
        dm_token: dm_token.clone(),
        ask_log: Mutex::new(HashMap::new()),
        campaign_asks: Mutex::new(Vec::new()),
        pin_log: Mutex::new(HashMap::new()),
        msg_log: Mutex::new(HashMap::new()),
    };
    Ok((players, dm_token))
}

fn save_tokens(dm: &str, by_token: &HashMap<String, PInfo>) -> Result<()> {
    let players: serde_json::Map<String, Value> = by_token
        .values()
        .map(|p| (p.slug.clone(), json!({ "token": p.token, "pin": p.pin })))
        .collect();
    std::fs::create_dir_all(".magehand")?;
    std::fs::write(TOKENS_PATH, json!({ "dm": dm, "players": players }).to_string())?;
    Ok(())
}

// ---------- player surface ----------

fn route_player(
    request: tiny_http::Request,
    method: &Method,
    seg: &str,
    players: &Arc<Players>,
    state: &Arc<Mutex<State>>,
    base: &Arc<Base>,
) {
    match (method, seg) {
        // the app page itself; fetches from it are relative, so they resolve under /p/
        (Method::Get, "") => match player_auth(&request, players) {
            Some(_) => {
                let _ = request.respond(Response::from_string(PLAYER_HTML).with_header(html_hdr()).with_header(nostore_hdr()));
            }
            None => reject(request),
        },
        // the API routes need the player cookie
        (Method::Get, "data") => match player_auth(&request, players) {
            Some(p) => respond_json(request, player_data(&p)),
            None => reject(request),
        },
        (Method::Post, "sheet") => match player_auth(&request, players) {
            Some(p) => {
                let st = Arc::clone(players);
                std::thread::spawn(move || player_sheet(request, &p, &st));
            }
            None => reject(request),
        },
        (Method::Post, "ask") => match player_auth(&request, players) {
            Some(p) => {
                let st = Arc::clone(players);
                std::thread::spawn(move || player_ask(request, &p, &st));
            }
            None => reject(request),
        },
        (Method::Get, "events") => match player_auth(&request, players) {
            Some(p) => match SseSlot::take(&p.slug, SSE_PER_PLAYER) {
                Some(slot) => {
                    let st = Arc::clone(state);
                    std::thread::spawn(move || {
                        let _slot = slot;
                        stream_player(request, &st, &p.slug);
                    });
                }
                None => {
                    let _ = request
                        .respond(Response::from_string("too many streams").with_status_code(429));
                }
            },
            None => reject(request),
        },
        (Method::Post, "msg") => match player_auth(&request, players) {
            Some(p) => {
                let st = Arc::clone(state);
                let pl = Arc::clone(players);
                std::thread::spawn(move || player_msg(request, &p, &pl, &st));
            }
            None => reject(request),
        },
        (Method::Get, "voice.js") => match player_auth(&request, players) {
            Some(_) => {
                let _ = request.respond(
                    Response::from_string(VOICE_JS)
                        .with_header(header("Content-Type", "application/javascript"))
                        .with_header(nostore_hdr()),
                );
            }
            None => reject(request),
        },
        (Method::Get, "rtc-config") => match player_auth(&request, players) {
            Some(_) => respond_json(request, rtc_config()),
            None => reject(request),
        },
        (Method::Post, "rtc") => match player_auth(&request, players) {
            Some(p) => {
                let st = Arc::clone(state);
                let pl = Arc::clone(players);
                std::thread::spawn(move || rtc_relay(request, &p.slug, &pl, &st));
            }
            None => reject(request),
        },
        (Method::Post, "audio") => match player_auth(&request, players) {
            Some(p) => {
                let st = Arc::clone(state);
                std::thread::spawn(move || post_audio(request, p.name, &st));
            }
            None => reject(request),
        },
        (Method::Get, "map-image") => match player_auth(&request, players) {
            Some(_) => serve_map_image(request),
            None => reject(request),
        },
        (Method::Post, "token") => match player_auth(&request, players) {
            Some(p) => {
                let st = Arc::clone(state);
                std::thread::spawn(move || player_map_token(request, &p, &st));
            }
            None => reject(request),
        },
        (Method::Post, "claim") => player_claim(request, players, base),
        (Method::Get, "claim") => {
            let known = cookie_of(&request, "mhc")
                .and_then(|tok| players.by_token.lock().unwrap().get(&tok).cloned());
            match known {
                Some(p) => {
                    let _ = request
                        .respond(Response::from_string(claim_page(&p, "")).with_header(html_hdr()).with_header(nostore_hdr()));
                }
                None => {
                    let _ = request.respond(Response::from_string("unknown player link").with_status_code(404));
                }
            }
        }
        // anything else under /p/ is a capability-token landing: stash the
        // token in a claim cookie and bounce, so the capability URL leaves
        // history/proxy logs before the PIN gate renders
        (Method::Get, tok) => {
            let tok = tok.split('?').next().unwrap_or(tok).to_string();
            let known = players.by_token.lock().unwrap().contains_key(&tok);
            if known {
                let _ = request.respond(
                    redirect(&format!("{}/p/claim", base.path))
                        .with_header(cookie_hdr("mhc", &tok, base)),
                );
            } else {
                let _ = request.respond(Response::from_string("unknown player link").with_status_code(404));
            }
        }
        _ => reject(request),
    }
}

/// The PIN gate between a join link and the cookie. First visit sets the PIN;
/// later visits (new device, or someone else holding a leaked link) must match
/// it. Attempts are throttled per token — that, not PIN secrecy, is the defense.
fn player_claim(mut request: tiny_http::Request, players: &Arc<Players>, base: &Arc<Base>) {
    let mut body = String::new();
    let cap = request.body_length().unwrap_or(0).min(MAX_BODY);
    let _ = request.as_reader().take(cap as u64).read_to_string(&mut body);
    let pin = body.split('&').find_map(|kv| {
        let (key, v) = kv.split_once('=')?;
        (key == "pin").then(|| v.to_string())
    });
    let (Some(tok), Some(pin)) = (cookie_of(&request, "mhc"), pin) else {
        let _ = request.respond(Response::from_string("bad claim").with_status_code(400));
        return;
    };
    // token lookup BEFORE the throttle: unknown tokens must not grow pin_log,
    // or unauthenticated spam becomes unbounded memory
    let Some(p) = players.by_token.lock().unwrap().get(&tok).cloned() else {
        let _ = request.respond(Response::from_string("unknown player link").with_status_code(404));
        return;
    };
    {
        let mut log = players.pin_log.lock().unwrap();
        let now = Instant::now();
        let hits = log.entry(tok.clone()).or_default();
        hits.retain(|t| now.duration_since(*t) < Duration::from_secs(3600));
        if hits.len() >= PIN_TRIES_PER_HOUR {
            let _ = request
                .respond(Response::from_string("too many tries — wait an hour").with_status_code(429));
            return;
        }
        hits.push(now);
    }
    if !(4..=6).contains(&pin.len()) || !pin.chars().all(|c| c.is_ascii_digit()) {
        let page = claim_page(&p, "PIN must be 4–6 digits");
        let _ = request.respond(Response::from_string(page).with_header(html_hdr()).with_header(nostore_hdr()));
        return;
    }
    if p.pin.is_empty() {
        let mut map = players.by_token.lock().unwrap();
        let Some(entry) = map.get_mut(&tok) else {
            drop(map);
            let _ = request.respond(Response::from_string("unknown player link").with_status_code(404));
            return;
        };
        entry.pin = pin;
        if let Err(e) = save_tokens(&players.dm_token, &map) {
            // unpersisted PIN must not gate future claims — roll back and report
            map.get_mut(&tok).expect("lock held since fetch").pin = String::new();
            drop(map);
            let page = claim_page(&p, &format!("couldn't save PIN ({e}) — try again"));
            let _ = request.respond(Response::from_string(page).with_header(html_hdr()).with_header(nostore_hdr()));
            return;
        }
    } else if p.pin != pin {
        let page = claim_page(&p, "wrong PIN — try again");
        let _ = request.respond(Response::from_string(page).with_header(html_hdr()).with_header(nostore_hdr()));
        return;
    }
    let _ = request.respond(
        redirect(&format!("{}/p/", base.path))
            .with_header(cookie_hdr("mhp", &tok, base))
            .with_header(clear_cookie_hdr("mhc", base)),
    );
}

fn claim_page(p: &PInfo, error: &str) -> String {
    let (title, hint) = if p.pin.is_empty() {
        ("Choose a PIN", "Pick a 4-digit PIN. You'll need it if you open your link on another device.")
    } else {
        ("Enter your PIN", "This link was already claimed — enter the PIN you chose.")
    };
    let err = if error.is_empty() { String::new() } else { format!("<p class=err>{}</p>", html_escape(error)) };
    format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content='width=device-width,initial-scale=1'>\
         <title>{title} — Magehand</title><style>\
         body{{background:#14131a;color:#ece9f5;font:16px system-ui,sans-serif;margin:0;display:grid;place-items:center;min-height:100dvh;}}\
         form{{background:#1e1c26;border:1px solid #2c2a38;border-radius:12px;padding:24px;max-width:320px;text-align:center;}}\
         h1{{font-size:20px;margin:0 0 6px;}} p{{color:#8a8699;font-size:14px;}} .err{{color:#e07777;}}\
         input{{font-size:24px;text-align:center;letter-spacing:6px;width:9ch;background:#14131a;color:#ece9f5;\
         border:1px solid #2c2a38;border-radius:8px;padding:8px;}}\
         button{{display:block;margin:16px auto 0;font-size:16px;padding:8px 24px;border-radius:8px;border:0;\
         background:#7c6cd0;color:#fff;}}</style>\
         <form method=post action=claim>\
         <h1>{} — {title}</h1><p>{hint}</p>{err}\
         <input name=pin inputmode=numeric pattern='[0-9]*' maxlength=6 autofocus autocomplete=off>\
         <button>Join</button></form>",
        html_escape(&p.name),
    )
}

/// DM action: re-mint one player's join link (leaked screenshot, wrong Discord
/// channel…). The PIN survives — it's the same player, new capability URL.
fn regen_link(mut request: tiny_http::Request, players: &Arc<Players>, dm_token: &str, base: &Arc<Base>) {
    let req = read_body(&mut request);
    let Some(slug) = req["slug"].as_str() else {
        respond_json(request, json!({ "ok": false, "msg": "need slug" }));
        return;
    };
    let Ok(new_tok) = mint_token() else {
        respond_json(request, json!({ "ok": false, "msg": "couldn't mint token" }));
        return;
    };
    let mut map = players.by_token.lock().unwrap();
    let Some(old) = map.values().find(|p| p.slug == slug).map(|p| p.token.clone()) else {
        respond_json(request, json!({ "ok": false, "msg": "no such player" }));
        return;
    };
    let mut p = map.remove(&old).expect("just found");
    p.token = new_tok.clone();
    map.insert(new_tok.clone(), p);
    let saved = save_tokens(dm_token, &map);
    drop(map);
    match saved {
        Ok(()) => respond_json(
            request,
            json!({ "ok": true, "msg": "old link is dead", "url": format!("{}/p/{new_tok}", base.url) }),
        ),
        Err(e) => respond_json(request, json!({ "ok": false, "msg": e.to_string() })),
    }
}

fn player_auth(request: &tiny_http::Request, players: &Players) -> Option<PInfo> {
    let tok = cookie_of(request, "mhp")?;
    players.by_token.lock().unwrap().get(&tok).cloned()
}

fn player_data(p: &PInfo) -> Value {
    let (fields, body) =
        sheets::read_sheet(&p.slug).unwrap_or_else(|| (Vec::new(), String::new()));
    let fields: Vec<Value> = fields
        .into_iter()
        .filter(|(k, _)| k != "kind" && k != "player")
        .map(|(k, v)| {
            let numeric = v.parse::<i64>().is_ok();
            json!({ "key": k, "value": v, "numeric": numeric })
        })
        .collect();
    json!({
        "name": p.name,
        "slug": p.slug,
        "has_sheet": !fields.is_empty(),
        "fields": fields,
        "body": body,
        "secrets": sheets::secrets_of(&p.slug),
        "recap": sheets::latest_recap(),
        "messages": msgs_for(&p.slug),
    })
}

fn player_sheet(mut request: tiny_http::Request, p: &PInfo, _players: &Arc<Players>) {
    let req = read_body(&mut request);
    let (Some(key), Some(value)) = (req["key"].as_str(), req["value"].as_str()) else {
        respond_json(request, json!({ "ok": false, "msg": "need key and value" }));
        return;
    };
    match sheets::set_field(&p.slug, key, value) {
        Ok(saved) => respond_json(request, json!({ "ok": true, "key": key, "value": saved })),
        Err(e) => respond_json(request, json!({ "ok": false, "msg": e.to_string() })),
    }
}

fn player_ask(mut request: tiny_http::Request, p: &PInfo, players: &Arc<Players>) {
    let req = read_body(&mut request);
    let question = req["q"].as_str().unwrap_or("").trim().to_string();
    if question.len() < 3 {
        respond_json(request, json!({ "ok": false, "answer": "ask a rules or lore question" }));
        return;
    }
    // per-player sliding-hour rate limit — cheap, but a rules-lawyer loop shouldn't run up cost
    {
        let mut log = players.ask_log.lock().unwrap();
        let now = Instant::now();
        let hits = log.entry(p.slug.clone()).or_default();
        hits.retain(|t| now.duration_since(*t) < Duration::from_secs(3600));
        if hits.len() >= ASK_PER_HOUR {
            drop(log);
            respond_json(request, json!({ "ok": false, "answer": "you've asked a lot this hour — give the DM a turn" }));
            return;
        }
        hits.push(now);
    }
    // campaign-wide sliding-day cap — one table can't drain the host's LLM key
    {
        let mut asks = players.campaign_asks.lock().unwrap();
        let now = Instant::now();
        asks.retain(|t| now.duration_since(*t) < Duration::from_secs(86_400));
        if asks.len() >= ASK_PER_DAY {
            drop(asks);
            respond_json(request, json!({ "ok": false, "answer": "the table hit today's question budget — ask your DM" }));
            return;
        }
        asks.push(now);
    }
    let result = (|| -> Result<String> {
        let conn = open_db()?;
        let llm = llm_config();
        answer(&conn, &llm, &mut Vec::new(), &question, true) // player=true: spoiler-safe retrieval
    })();
    let payload = match result {
        Ok(a) => json!({ "ok": true, "answer": a }),
        Err(e) => json!({ "ok": false, "answer": format!("couldn't answer ({e})") }),
    };
    respond_json(request, payload);
}

// ---------- map ----------

/// Serializes read-modify-write of maps/state.json (two token drags landing at
/// once must not lose one), mirroring the SHEET_WRITE idiom in sheets.rs.
static MAP_WRITE: Mutex<()> = Mutex::new(());

fn map_dir() -> String {
    format!("{CAMPAIGN}/maps")
}

fn map_state() -> Value {
    read_lossy(Path::new(&format!("{}/state.json", map_dir())))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!({ "image": Value::Null, "v": 0, "tokens": [] }))
}

fn mutate_map(
    state: &Arc<Mutex<State>>,
    f: impl FnOnce(&mut Value) -> Result<()>,
) -> Result<()> {
    let _guard = MAP_WRITE.lock().unwrap();
    let mut map = map_state();
    f(&mut map)?;
    std::fs::create_dir_all(map_dir())?;
    std::fs::write(format!("{}/state.json", map_dir()), map.to_string())?;
    state.lock().unwrap().map_json = map.to_string();
    Ok(())
}

/// The image filename is always server-authored (`current.<ext>`) — clients
/// never send a path, so there is nothing to traverse.
fn serve_map_image(request: tiny_http::Request) {
    let map = map_state();
    let Some(name) = map["image"].as_str() else {
        let _ = request.respond(Response::from_string("no map yet").with_status_code(404));
        return;
    };
    let ct = match name.rsplit('.').next() {
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        _ => "image/jpeg",
    };
    match std::fs::read(format!("{}/{name}", map_dir())) {
        Ok(bytes) => {
            let _ = request.respond(Response::from_data(bytes).with_header(header("Content-Type", ct)));
        }
        Err(_) => {
            let _ = request.respond(Response::from_string("no map file").with_status_code(404));
        }
    }
}

fn upload_map_image(mut request: tiny_http::Request, state: &Arc<Mutex<State>>) {
    let ct = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Content-Type"))
        .map(|h| h.value.as_str().to_string())
        .unwrap_or_default();
    let ext = match ct.as_str() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        _ => {
            respond_json(request, json!({ "ok": false, "msg": "png, jpeg, or webp only" }));
            return;
        }
    };
    let len = request.body_length().unwrap_or(0);
    if len == 0 || len > MAX_IMAGE {
        respond_json(request, json!({ "ok": false, "msg": "image must be under 8 MB" }));
        return;
    }
    let mut buf = Vec::with_capacity(len);
    if request.as_reader().take(MAX_IMAGE as u64).read_to_end(&mut buf).is_err() {
        respond_json(request, json!({ "ok": false, "msg": "upload failed" }));
        return;
    }
    let result = mutate_map(state, |map| {
        std::fs::create_dir_all(map_dir())?;
        std::fs::write(format!("{}/current.{ext}", map_dir()), &buf)?;
        map["image"] = json!(format!("current.{ext}"));
        map["v"] = json!(map["v"].as_u64().unwrap_or(0) + 1); // cache-buster for the <img>
        Ok(())
    });
    respond_json(request, result_json(result, "map updated"));
}

fn dm_map_token(mut request: tiny_http::Request, state: &Arc<Mutex<State>>) {
    let req = read_body(&mut request);
    let result = mutate_map(state, |map| {
        let tokens = map["tokens"].as_array_mut().ok_or("bad map state")?;
        match req["op"].as_str().unwrap_or("") {
            "add" => {
                let label = req["label"].as_str().unwrap_or("").trim().chars().take(40).collect::<String>();
                if label.is_empty() {
                    return Err("token needs a label".into());
                }
                let base_id = crate::campaign::slugify(&label);
                let mut id = base_id.clone();
                let mut n = 1;
                while tokens.iter().any(|t| t["id"] == id.as_str()) {
                    n += 1;
                    id = format!("{base_id}-{n}");
                }
                let owner = req["owner"].as_str().unwrap_or("");
                tokens.push(json!({
                    "id": id, "label": label, "owner": owner,
                    "x": frac(&req["x"], 0.5), "y": frac(&req["y"], 0.5),
                }));
            }
            "move" => {
                let t = tokens
                    .iter_mut()
                    .find(|t| t["id"] == req["id"])
                    .ok_or("no such token")?;
                t["x"] = json!(frac(&req["x"], 0.5));
                t["y"] = json!(frac(&req["y"], 0.5));
            }
            "remove" => tokens.retain(|t| t["id"] != req["id"]),
            _ => return Err("unknown map op".into()),
        }
        Ok(())
    });
    respond_json(request, result_json(result, "ok"));
}

/// Players may move exactly their own token — ownership checked server-side.
fn player_map_token(mut request: tiny_http::Request, p: &PInfo, state: &Arc<Mutex<State>>) {
    let req = read_body(&mut request);
    let slug = p.slug.clone();
    let result = mutate_map(state, |map| {
        let tokens = map["tokens"].as_array_mut().ok_or("bad map state")?;
        let t = tokens
            .iter_mut()
            .find(|t| t["id"] == req["id"] && t["owner"] == slug.as_str())
            .ok_or("that token isn't yours")?;
        t["x"] = json!(frac(&req["x"], 0.5));
        t["y"] = json!(frac(&req["y"], 0.5));
        Ok(())
    });
    respond_json(request, result_json(result, "ok"));
}

fn frac(v: &Value, default: f64) -> f64 {
    v.as_f64().unwrap_or(default).clamp(0.0, 1.0)
}

fn result_json(result: Result<()>, ok_msg: &str) -> Value {
    match result {
        Ok(()) => json!({ "ok": true, "msg": ok_msg }),
        Err(e) => json!({ "ok": false, "msg": e.to_string() }),
    }
}

// ---------- direct-to-DM messages ----------

fn msgs_path() -> String {
    format!(".magehand/messages-{}.jsonl", crate::campaign::today())
}

/// Session-scoped whisper ledger, not canon — same tier as the cards JSONL.
fn append_msg(from: &str, to: &str, text: &str) -> Result<()> {
    std::fs::create_dir_all(".magehand")?;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(msgs_path())?;
    let line = json!({ "ts": crate::listen::now_hms(), "from": from, "to": to, "text": text });
    f.write_all(format!("{line}\n").as_bytes())?;
    Ok(())
}

fn msgs_for(slug: &str) -> Vec<Value> {
    read_lossy(Path::new(&msgs_path()))
        .map(|t| {
            t.lines()
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .filter(|m| m["from"] == slug || m["to"] == slug)
                .collect()
        })
        .unwrap_or_default()
}

/// Player → DM whisper: lands as a card in the DM feed (reply is a card action).
fn player_msg(
    mut request: tiny_http::Request,
    p: &PInfo,
    players: &Arc<Players>,
    state: &Arc<Mutex<State>>,
) {
    let req = read_body(&mut request);
    let text: String = req["text"].as_str().unwrap_or("").trim().chars().take(500).collect();
    if text.is_empty() {
        respond_json(request, json!({ "ok": false, "msg": "empty message" }));
        return;
    }
    {
        let mut log = players.msg_log.lock().unwrap();
        let now = Instant::now();
        let hits = log.entry(p.slug.clone()).or_default();
        hits.retain(|t| now.duration_since(*t) < Duration::from_secs(3600));
        if hits.len() >= MSG_PER_HOUR {
            drop(log);
            respond_json(request, json!({ "ok": false, "msg": "slow down" }));
            return;
        }
        hits.push(now);
    }
    if let Err(e) = append_msg(&p.slug, "dm", &text) {
        respond_json(request, json!({ "ok": false, "msg": e.to_string() }));
        return;
    }
    let mut st = state.lock().unwrap();
    let id = st.next_id;
    st.next_id += 1;
    st.log.push(Ev::Card {
        id,
        card: json!({
            "signal": "whisper",
            "ts": crate::listen::now_hms(),
            "headline": format!("{} whispers", p.name),
            "body": text,
            "ref": p.slug,
            "live": true,
        }),
    });
    drop(st);
    respond_json(request, json!({ "ok": true }));
}

/// One SSE stream per player: DM replies now; map and RTC events later. Polls
/// faster than the DM stream — this channel will carry WebRTC signaling, where
/// 700 ms hops make call setup feel broken.
fn stream_player(request: tiny_http::Request, state: &Arc<Mutex<State>>, slug: &str) {
    let mut w = request.into_writer();
    let head = "HTTP/1.1 200 OK\r\n\
                Content-Type: text/event-stream\r\n\
                Cache-Control: no-cache\r\n\
                Connection: keep-alive\r\n\
                X-Accel-Buffering: no\r\n\r\n";
    if w.write_all(head.as_bytes()).is_err() {
        return;
    }
    let mut cursor = 0usize;
    let mut sent_map = String::new();
    let mut last_beat = Instant::now();
    loop {
        let frames = {
            let st = state.lock().unwrap();
            let mut out = String::new();
            if let Some(log) = st.plogs.get(slug) {
                for (event, data) in &log[cursor.min(log.len())..] {
                    out.push_str(&sse(event, &data.to_string()));
                }
                cursor = log.len();
            }
            if st.map_json != sent_map && !st.map_json.is_empty() {
                sent_map = st.map_json.clone();
                out.push_str(&sse("map", &sent_map));
            }
            out
        };
        if !frames.is_empty() && w.write_all(frames.as_bytes()).is_err() {
            return;
        }
        if last_beat.elapsed() > Duration::from_secs(15) {
            last_beat = Instant::now();
            if w.write_all(b": beat\n\n").is_err() {
                return;
            }
        }
        if w.flush().is_err() {
            return;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// DM-shown page: one QR per player linking to their capability URL.
fn join_page(players: &Players, base: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut cards = String::new();
    let map = players.by_token.lock().unwrap();
    let mut sorted: Vec<&PInfo> = map.values().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    for p in sorted {
        let url = format!("{base}/p/{}", p.token);
        let qr = qr_svg(&url);
        cards.push_str(&format!(
            "<div class=card><h2>{}</h2>{qr}<p class=url>{url}</p></div>",
            html_escape(&p.name)
        ));
    }
    if cards.is_empty() {
        cards = "<p>No players yet — add a sheet, backstory, or secret file, then restart serve.</p>".into();
    }
    let html = format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content='width=device-width,initial-scale=1'>\
         <title>Join — Magehand</title><style>\
         body{{background:#14131a;color:#ece9f5;font:16px system-ui,sans-serif;margin:0;padding:20px;}}\
         h1{{font-size:20px;}} .grid{{display:flex;flex-wrap:wrap;gap:20px;}}\
         .card{{background:#1e1c26;border:1px solid #2c2a38;border-radius:12px;padding:16px;text-align:center;}}\
         .card h2{{font-size:18px;margin:0 0 10px;}} .card svg{{width:180px;height:180px;background:#fff;border-radius:8px;padding:8px;}}\
         .url{{color:#8a8699;font-size:11px;word-break:break-all;max-width:196px;margin:8px auto 0;}}\
         </style><h1>Scan to join — one code per player</h1><div class=grid>{cards}</div>"
    );
    Response::from_string(html)
}

fn qr_svg(data: &str) -> String {
    use qrcode::render::svg;
    use qrcode::QrCode;
    match QrCode::new(data.as_bytes()) {
        Ok(code) => code
            .render::<svg::Color>()
            .min_dimensions(180, 180)
            .dark_color(svg::Color("#000"))
            .light_color(svg::Color("#fff"))
            .build(),
        Err(_) => "<p>(QR too long)</p>".into(),
    }
}

// ---------- shared state ----------

/// Append-only broadcast log of card/dismiss events (bounded to a session's
/// cards), plus replaced-in-place snapshots for threads and the transcript tail.
#[derive(Default)]
struct State {
    log: Vec<Ev>,
    next_id: u64,
    processed: usize,     // cards-jsonl lines already turned into Card events
    acted: HashSet<u64>,  // card ids already actioned — makes taps idempotent
    threads_json: String,
    transcript: String,
    party_json: String,
    /// Per-player push feeds (slug → (event, data) log): DM replies now; map
    /// updates and RTC signaling ride the same stream later. Session-bounded.
    plogs: HashMap<String, Vec<(String, Value)>>,
    /// Current map state as JSON — mirrors maps/state.json, diff-pushed to
    /// every stream (DM and players) whenever a mutation lands.
    map_json: String,
    /// WebRTC signaling addressed to the DM (players' feeds ride plogs).
    dm_rtc: Vec<Value>,
    /// The live transcription session, when one is running.
    live: Option<Live>,
    /// True while `session end` finalizes with the lock released — a new start
    /// must not reopen the live path before the old one is renamed away.
    session_ending: bool,
}

/// A running online session: browsers POST silence-gated WAV utterances, one
/// worker transcribes them in arrival order (ordering beats parallelism for a
/// transcript; ponytail: more workers per campaign if a backlog ever shows),
/// and the existing cards engine consumes the labeled lines unchanged.
struct Live {
    jobs: std::sync::mpsc::SyncSender<Job>,
    live_path: String,
    handles: Vec<std::thread::JoinHandle<()>>,
}

struct Job {
    speaker: String,
    wav: Vec<u8>,
    ooc: bool,
}

enum Ev {
    Card { id: u64, card: Value },
    Dismiss(u64),
}

fn spawn_poller(state: Arc<Mutex<State>>) {
    std::thread::spawn(move || loop {
        let cards_path = format!("{CARDS_DIR}/cards-{}.jsonl", crate::campaign::today());
        // new cards
        if let Ok(text) = read_lossy(Path::new(&cards_path)) {
            let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
            let mut st = state.lock().unwrap();
            while st.processed < lines.len() {
                let line = lines[st.processed];
                match serde_json::from_str::<Value>(line) {
                    Ok(card) => {
                        st.processed += 1;
                        if card["live"].as_bool() == Some(true) {
                            let id = st.next_id;
                            st.next_id += 1;
                            st.log.push(Ev::Card { id, card });
                        }
                    }
                    Err(_) if st.processed == lines.len() - 1 => {
                        break; // last line: likely a partial trailing write, retry next tick
                    }
                    Err(_) => {
                        // a complete line that will never parse — skip it rather
                        // than wedging every card written after it for the session
                        eprintln!("serve: skipping unparseable card line {}", st.processed + 1);
                        st.processed += 1;
                    }
                }
            }
        }
        // threads/party snapshots + transcript tail (cheap file reads; replace on change)
        let threads = read_threads();
        let transcript = read_transcript();
        let party = read_party();
        {
            let mut st = state.lock().unwrap();
            if threads != st.threads_json {
                st.threads_json = threads;
            }
            if transcript != st.transcript {
                st.transcript = transcript;
            }
            if party != st.party_json {
                st.party_json = party;
            }
        }
        std::thread::sleep(POLL);
    });
}

// ---------- SSE ----------

fn stream_events(request: tiny_http::Request, state: Arc<Mutex<State>>) {
    let mut w = request.into_writer();
    let head = "HTTP/1.1 200 OK\r\n\
                Content-Type: text/event-stream\r\n\
                Cache-Control: no-cache\r\n\
                Connection: keep-alive\r\n\
                X-Accel-Buffering: no\r\n\r\n";
    if w.write_all(head.as_bytes()).is_err() {
        return;
    }
    let mut cursor = 0usize;
    let mut rtc_cursor = 0usize;
    let mut sent_live: Option<bool> = None;
    let mut sent_threads = String::new();
    let mut sent_transcript = String::new();
    let mut sent_party = String::new();
    let mut sent_map = String::new();
    let mut last_beat = Instant::now();
    loop {
        let (frames, beat) = {
            let st = state.lock().unwrap();
            let mut out = String::new();
            for ev in &st.log[cursor.min(st.log.len())..] {
                match ev {
                    Ev::Card { id, card } => {
                        let mut c = card.clone();
                        c["id"] = json!(id);
                        out.push_str(&sse("card", &c.to_string()));
                    }
                    Ev::Dismiss(id) => out.push_str(&sse("dismiss", &json!({ "id": id }).to_string())),
                }
            }
            cursor = st.log.len();
            for msg in &st.dm_rtc[rtc_cursor.min(st.dm_rtc.len())..] {
                out.push_str(&sse("rtc", &msg.to_string()));
            }
            rtc_cursor = st.dm_rtc.len();
            let live_now = st.live.is_some();
            if sent_live != Some(live_now) {
                sent_live = Some(live_now);
                out.push_str(&sse("session", &json!({ "live": live_now }).to_string()));
            }
            if st.threads_json != sent_threads && !st.threads_json.is_empty() {
                sent_threads = st.threads_json.clone();
                out.push_str(&sse("threads", &sent_threads));
            }
            if st.party_json != sent_party && !st.party_json.is_empty() {
                sent_party = st.party_json.clone();
                out.push_str(&sse("party", &sent_party));
            }
            if st.map_json != sent_map && !st.map_json.is_empty() {
                sent_map = st.map_json.clone();
                out.push_str(&sse("map", &sent_map));
            }
            if st.transcript != sent_transcript {
                sent_transcript = st.transcript.clone();
                out.push_str(&sse("transcript", &json!({ "text": sent_transcript }).to_string()));
            }
            (out, last_beat.elapsed() > Duration::from_secs(15))
        };
        if !frames.is_empty() && w.write_all(frames.as_bytes()).is_err() {
            return;
        }
        if beat {
            last_beat = Instant::now();
            if w.write_all(b": beat\n\n").is_err() {
                return; // client gone
            }
        }
        if w.flush().is_err() {
            return;
        }
        std::thread::sleep(POLL);
    }
}

fn sse(event: &str, data: &str) -> String {
    // data is single-line JSON, so no multi-line framing needed
    format!("event: {event}\ndata: {data}\n\n")
}

// ---------- actions ----------

/// One tap → one existing vault command. Nothing writes without a tap; the
/// card is claimed under the lock BEFORE any write, so a double-tap or two
/// devices tapping the same card can't double-write the vault.
fn handle_action(mut request: tiny_http::Request, state: Arc<Mutex<State>>) {
    let mut body = String::new();
    let cap = request.body_length().unwrap_or(0).min(MAX_BODY);
    let _ = request.as_reader().take(cap as u64).read_to_string(&mut body);
    let req: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let action = req["action"].as_str().unwrap_or("").to_string();

    // claim the card: find it and mark it acted atomically, so a concurrent or
    // repeated tap on the same id is rejected before any file write happens
    let claimed = req["id"].as_u64().and_then(|id| {
        let mut st = state.lock().unwrap();
        let card = st.log.iter().find_map(|ev| match ev {
            Ev::Card { id: cid, card } if *cid == id => Some(card.clone()),
            _ => None,
        })?;
        if st.acted.insert(id) {
            Some((id, card))
        } else {
            None // already handled (or being handled)
        }
    });
    let Some((id, card)) = claimed else {
        let msg = if req["id"].as_u64().is_none() { "no such card" } else { "already handled" };
        respond_json(request, json!({ "ok": false, "msg": msg }));
        return;
    };

    let headline = card["headline"].as_str().unwrap_or("").trim();
    let result: Result<String> = match action.as_str() {
        "dismiss" => Ok("dismissed".into()),
        "reply" => {
            let text: String = req["text"].as_str().unwrap_or("").trim().chars().take(500).collect();
            let to = card["ref"].as_str().unwrap_or("").to_string();
            if text.is_empty() || to.is_empty() {
                Err("need reply text".into())
            } else {
                append_msg("dm", &to, &text).map(|()| {
                    let ev = json!({ "ts": crate::listen::now_hms(), "text": text });
                    state.lock().unwrap().plogs.entry(to).or_default().push(("dm".into(), ev));
                    "sent".into()
                })
            }
        }
        "ruling" => {
            let body = card["body"].as_str().unwrap_or("");
            let text = if body.is_empty() { headline.to_string() } else { format!("{headline} — {body}") };
            if text.trim().is_empty() {
                Err("card has no text to record".into())
            } else {
                cmd_ruling(&text).map(|_| "saved as a table ruling".into())
            }
        }
        "thread" => {
            if headline.is_empty() {
                Err("card has no title for a thread".into())
            } else {
                match cmd_thread(&["add".to_string(), headline.to_string()]) {
                    Ok(()) => Ok("opened a thread".into()),
                    Err(e) if e.to_string().contains("already exists") => Ok("thread already open".into()),
                    Err(e) => Err(e),
                }
            }
        }
        other => Err(format!("unknown action `{other}`").into()),
    };

    let mut st = state.lock().unwrap();
    let payload = match result {
        Ok(msg) => {
            st.log.push(Ev::Dismiss(id)); // acted-on card leaves every feed
            json!({ "ok": true, "msg": msg })
        }
        Err(e) => {
            st.acted.remove(&id); // write failed — let the DM retry this card
            json!({ "ok": false, "msg": e.to_string() })
        }
    };
    drop(st);
    respond_json(request, payload);
}

fn respond_json(request: tiny_http::Request, payload: Value) {
    let _ = request.respond(Response::from_string(payload.to_string()).with_header(json_hdr()));
}

// ---------- voice / transcription ----------

fn rtc_config() -> Value {
    let mut servers = vec![json!({ "urls": "stun:stun.l.google.com:19302" })];
    if let Ok(url) = std::env::var("MAGEHAND_TURN_URL") {
        servers.push(json!({
            "urls": url,
            "username": std::env::var("MAGEHAND_TURN_USER").unwrap_or_default(),
            "credential": std::env::var("MAGEHAND_TURN_PASS").unwrap_or_default(),
        }));
    }
    json!({ "iceServers": servers })
}

/// Relay WebRTC signaling. `from` is stamped server-side from the caller's
/// cookie, so a client can never speak as someone else.
fn rtc_relay(
    mut request: tiny_http::Request,
    from: &str,
    players: &Arc<Players>,
    state: &Arc<Mutex<State>>,
) {
    let req = read_body(&mut request);
    let Some(to) = req["to"].as_str().map(str::to_string) else {
        respond_json(request, json!({ "ok": false, "msg": "need to" }));
        return;
    };
    let msg = json!({ "from": from, "type": req["type"], "payload": req["payload"] });
    // roster snapshot BEFORE the State lock — the two locks must never nest
    let slugs: Vec<String> =
        { players.by_token.lock().unwrap().values().map(|p| p.slug.clone()).collect() };
    if to != "*" && to != "dm" && !slugs.contains(&to) {
        respond_json(request, json!({ "ok": false, "msg": "no such peer" }));
        return;
    }
    let mut st = state.lock().unwrap();
    match to.as_str() {
        // broadcast: skip peers whose backlog is full rather than failing the
        // whole mesh because one absent reader stopped draining
        "*" => {
            for slug in slugs {
                if slug != from {
                    let log = st.plogs.entry(slug).or_default();
                    if log.len() < RTC_BACKLOG {
                        log.push(("rtc".into(), msg.clone()));
                    }
                }
            }
            if from != "dm" && st.dm_rtc.len() < RTC_BACKLOG {
                st.dm_rtc.push(msg);
            }
        }
        "dm" => {
            if st.dm_rtc.len() >= RTC_BACKLOG {
                drop(st);
                respond_json(request, json!({ "ok": false, "msg": "peer backlog full" }));
                return;
            }
            st.dm_rtc.push(msg);
        }
        other => {
            if st.plogs.get(other).is_some_and(|log| log.len() >= RTC_BACKLOG) {
                drop(st);
                respond_json(request, json!({ "ok": false, "msg": "peer backlog full" }));
                return;
            }
            st.plogs.entry(other.to_string()).or_default().push(("rtc".into(), msg));
        }
    }
    drop(st);
    respond_json(request, json!({ "ok": true }));
}

fn post_audio(mut request: tiny_http::Request, speaker: String, state: &Arc<Mutex<State>>) {
    // advisory out-of-character flag set by the browser's OOC toggle
    let ooc = request
        .headers()
        .iter()
        .any(|h| h.field.equiv("X-OOC") && h.value.as_str() == "1");
    let len = request.body_length().unwrap_or(0);
    if len == 0 || len > MAX_WAV {
        respond_json(request, json!({ "ok": false, "msg": "utterance must be under 2 MB" }));
        return;
    }
    let mut wav = Vec::with_capacity(len);
    if request.as_reader().take(MAX_WAV as u64).read_to_end(&mut wav).is_err() {
        respond_json(request, json!({ "ok": false, "msg": "upload failed" }));
        return;
    }
    let sent = {
        let st = state.lock().unwrap();
        match &st.live {
            None => Err("no session running"),
            // full queue → drop this utterance rather than block the request
            // thread; the browser keeps talking and later clips still land
            Some(live) => {
                live.jobs.try_send(Job { speaker, wav, ooc }).map_err(|_| "transcriber busy")
            }
        }
    };
    match sent {
        Ok(()) => respond_json(request, json!({ "ok": true })),
        Err(msg) => respond_json(request, json!({ "ok": false, "msg": msg })),
    }
}

fn session_ctl(mut request: tiny_http::Request, state: &Arc<Mutex<State>>) {
    let req = read_body(&mut request);
    let result = match req["op"].as_str().unwrap_or("") {
        "start" => session_start(state),
        "end" => session_end(state),
        _ => Err("op must be start or end".into()),
    };
    match result {
        Ok(msg) => respond_json(request, json!({ "ok": true, "msg": msg })),
        Err(e) => respond_json(request, json!({ "ok": false, "msg": e.to_string() })),
    }
}

fn session_start(state: &Arc<Mutex<State>>) -> Result<String> {
    {
        let st = state.lock().unwrap();
        if st.live.is_some() {
            return Err("a session is already running".into());
        }
        if st.session_ending {
            return Err("previous session still archiving".into());
        }
    }
    let lexicon = crate::listen::build_lexicon();
    let hotwords = crate::listen::hotword_names(&lexicon);
    let live_path = format!("{CAMPAIGN}/sessions/{}-live.md", crate::campaign::today());
    // two racing starts both pass the check above; open_live's flock rejects
    // the second one here, so at most one session ever owns the transcript
    let mut live_file = crate::listen::open_live(&live_path)?;
    let listener = crate::signals::Listener::new(false)?;
    let (line_tx, line_rx) = std::sync::mpsc::sync_channel::<String>(64);
    let listener_handle = std::thread::spawn(move || {
        let mut listener = listener;
        for line in line_rx {
            listener.push_line(&line);
        }
        listener.finish(false);
    });
    let (jobs, job_rx) = std::sync::mpsc::sync_channel::<Job>(32);
    let url = std::env::var("MAGEHAND_STT_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:9090/inference".into());
    // one worker: transcript order beats parallelism
    let err_state = Arc::clone(state);
    let worker = std::thread::spawn(move || {
        let mut last_err_card: Option<Instant> = None;
        for job in job_rx {
            let text = match transcribe(&url, &job.wav, &hotwords) {
                Ok(t) => crate::listen::clean_stt_line(&t),
                Err(e) => {
                    eprintln!("serve: transcription failed: {e}");
                    // surface a dead whisper-server on the DM feed, but only
                    // one card per 5 minutes — every utterance fails at once
                    if last_err_card.is_none_or(|t| t.elapsed() > Duration::from_secs(300)) {
                        last_err_card = Some(Instant::now());
                        let body: String = e.to_string().chars().take(200).collect();
                        let mut st = err_state.lock().unwrap();
                        let id = st.next_id;
                        st.next_id += 1;
                        st.log.push(Ev::Card {
                            id,
                            card: json!({
                                "signal": "trigger",
                                "ts": crate::listen::now_hms(),
                                "headline": "transcription failing",
                                "body": body,
                                "live": true,
                            }),
                        });
                    }
                    continue;
                }
            };
            if text.is_empty() {
                continue;
            }
            let marker = if job.ooc { " (ooc)" } else { "" };
            let line = format!("{}{marker}: {text}", job.speaker);
            if let Err(e) = crate::listen::append_line(&mut live_file, &line) {
                eprintln!("serve: couldn't write transcript: {e}");
            }
            let _ = line_tx.try_send(line); // analysis may lag; the transcript is complete
        }
        // live_file drops here, releasing the flock before finalize renames it
    });
    state.lock().unwrap().live =
        Some(Live { jobs, live_path, handles: vec![worker, listener_handle] });
    Ok("session started".into())
}

fn session_end(state: &Arc<Mutex<State>>) -> Result<String> {
    let live = {
        let mut st = state.lock().unwrap();
        let live = st.live.take().ok_or("no session running")?;
        st.session_ending = true; // same critical section as the take — no start can slip between
        live
    };
    struct Ending<'a>(&'a Mutex<State>);
    impl Drop for Ending<'_> {
        fn drop(&mut self) {
            self.0.lock().unwrap().session_ending = false;
        }
    }
    let _ending = Ending(state);
    let Live { jobs, live_path, handles } = live;
    drop(jobs); // worker drains the queue and exits; its line sender then ends the listener
    for h in handles {
        h.join().map_err(|_| "session thread panicked")?;
    }
    crate::listen::finalize(&live_path, &crate::listen::build_lexicon(), true)?;
    Ok("session ended".into())
}

/// Hand-built multipart POST to whisper-server's /inference — one static
/// boundary is fine because WAV bytes can't contain it and the text parts are
/// our own.
fn transcribe(url: &str, wav: &[u8], prompt: &str) -> Result<String> {
    const B: &str = "----magehand7f3a9c1e";
    let mut body = Vec::with_capacity(wav.len() + 512);
    body.extend_from_slice(
        format!(
            "--{B}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"u.wav\"\r\n\
             Content-Type: audio/wav\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(wav);
    for (name, value) in [("response_format", "json"), ("prompt", prompt)] {
        body.extend_from_slice(
            format!("\r\n--{B}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}")
                .as_bytes(),
        );
    }
    body.extend_from_slice(format!("\r\n--{B}--\r\n").as_bytes());
    let resp = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(60))
        .build()
        .post(url)
        .set("Content-Type", &format!("multipart/form-data; boundary={B}"))
        .send_bytes(&body)
        .map_err(|e| format!("whisper-server: {e}"))?;
    let v: Value = resp.into_json()?;
    v["text"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("unexpected STT response: {v}").into())
}

// ---------- vault reads ----------

fn read_threads() -> String {
    let day = current_day();
    let mut out = Vec::new();
    for p in md_files(&format!("{CAMPAIGN}/threads")) {
        let Ok(text) = read_lossy(&p) else { continue };
        if fm_value(&text, "status").as_deref() != Some("open") {
            continue;
        }
        let slug = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let title = text.lines().find_map(|l| l.strip_prefix("# ")).unwrap_or(&slug).to_string();
        let due = fm_value(&text, "due").and_then(|v| v.parse::<i64>().ok());
        let overdue = matches!((due, day), (Some(d), Some(now)) if d <= now);
        out.push(json!({ "slug": slug, "title": title, "due": due, "overdue": overdue }));
    }
    // overdue first, then by title, so the pinned strip leads with what's live
    out.sort_by(|a, b| {
        b["overdue"].as_bool().cmp(&a["overdue"].as_bool())
            .then(a["title"].as_str().cmp(&b["title"].as_str()))
    });
    Value::Array(out).to_string()
}

/// The glanceable numbers a DM checks mid-combat, one object per PC.
fn read_party() -> String {
    const KEYS: &[&str] = &[
        "class", "level", "cur_hp", "max_hp", "temp_hp", "ac", "passive_perception",
        "conditions", "death_success", "death_fail", "inspiration",
    ];
    let mut out = Vec::new();
    for slug in sheets::roster() {
        let Some((fm, _)) = sheets::read_sheet(&slug) else { continue };
        let mut o = serde_json::Map::new();
        o.insert("name".into(), json!(sheets::display_name(&slug)));
        for (k, v) in fm {
            if KEYS.contains(&k.as_str()) {
                o.insert(k, json!(v));
            }
        }
        out.push(Value::Object(o));
    }
    Value::Array(out).to_string()
}

fn read_transcript() -> String {
    let path = format!("{CAMPAIGN}/sessions/{}-live.md", crate::campaign::today());
    let Ok(text) = read_lossy(Path::new(&path)) else { return String::new() };
    text.lines()
        .rev()
        .find_map(|l| l.strip_prefix("- ["))
        .and_then(|l| l.split_once(']').map(|(_, t)| t.trim().to_string()))
        .unwrap_or_default()
}

// ---------- http helpers ----------

fn arg_val<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).map(String::as_str)
}

fn mint_token() -> Result<String> {
    let mut buf = [0u8; 16];
    let mut f = std::fs::File::open("/dev/urandom")?;
    f.read_exact(&mut buf)?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// LAN IP without a dep: a UDP socket "connected" to a public addr resolves the
/// local outbound interface — no packet is actually sent.
fn lan_url(port: u16) -> String {
    let ip = std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("8.8.8.8:80")?;
            Ok(s.local_addr()?.ip().to_string())
        })
        .unwrap_or_else(|_| "localhost".into());
    format!("http://{ip}:{port}")
}

fn authed(request: &tiny_http::Request, token: &str) -> bool {
    cookie_of(request, "mh").as_deref() == Some(token)
}

fn cookie_of(request: &tiny_http::Request, name: &str) -> Option<String> {
    let raw = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Cookie"))
        .map(|h| h.value.as_str())?;
    raw.split(';').find_map(|kv| {
        let (k, v) = kv.trim().split_once('=')?;
        (k == name).then(|| v.to_string())
    })
}

fn reject(request: tiny_http::Request) {
    let _ = request.respond(Response::from_string("unauthorized").with_status_code(401));
}

fn read_body(request: &mut tiny_http::Request) -> Value {
    let cap = request.body_length().unwrap_or(0).min(MAX_BODY);
    let mut body = String::new();
    let _ = request.as_reader().take(cap as u64).read_to_string(&mut body);
    serde_json::from_str(&body).unwrap_or(Value::Null)
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn query_token(url: &str) -> Option<String> {
    let q = url.split('?').nth(1)?;
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == "t").then(|| v.to_string())
    })
}

fn header(field: &str, value: &str) -> Header {
    Header::from_bytes(field.as_bytes(), value.as_bytes()).expect("valid header")
}

fn html_hdr() -> Header {
    header("Content-Type", "text/html; charset=utf-8")
}

fn json_hdr() -> Header {
    header("Content-Type", "application/json")
}

/// tiny_http sends no cache headers at all, and Chrome will happily reuse a
/// header-less page across server restarts — token-gated pages must never come
/// from disk cache.
fn nostore_hdr() -> Header {
    header("Cache-Control", "no-store")
}

fn clear_cookie_hdr(name: &str, base: &Base) -> Header {
    let secure = if base.secure { "; Secure" } else { "" };
    header(
        "Set-Cookie",
        &format!("{name}=; Path={}/; SameSite=Lax; HttpOnly; Max-Age=0{secure}", base.path),
    )
}

fn cookie_hdr(name: &str, token: &str, base: &Base) -> Header {
    // Path-scoped so two campaigns behind one domain can't clobber each other's
    // cookies; HttpOnly keeps page JS away from the capability token; Secure
    // only when the public base is https (plain-LAN serve still works).
    let secure = if base.secure { "; Secure" } else { "" };
    header(
        "Set-Cookie",
        &format!("{name}={token}; Path={}/; SameSite=Lax; HttpOnly{secure}", base.path),
    )
}
