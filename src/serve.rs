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
const POLL: Duration = Duration::from_millis(700);
const MAX_BODY: usize = 16 * 1024;
const ASK_PER_HOUR: usize = 12;

/// Player roster + capability tokens (immutable after startup) and per-player
/// ask rate limiting.
struct Players {
    by_token: HashMap<String, PInfo>,
    ask_log: Mutex<HashMap<String, Vec<Instant>>>,
}

#[derive(Clone)]
struct PInfo {
    slug: String,
    name: String,
    token: String,
}

/// Phase 3 of table mode: the DM dashboard. A LAN web page (token-gated, since
/// it shows secrets) that tails the listener's card JSONL over SSE and turns
/// one tap into an existing vault command. Decoupled from `listen` by design —
/// the cards file is the bus, so a dashboard crash never touches transcription.
pub(crate) fn cmd_serve(args: &[String]) -> Result<()> {
    ensure_vault()?;
    let port: u16 = arg_val(args, "--port").and_then(|v| v.parse().ok()).unwrap_or(7979);
    let token = mint_token()?;
    let server = Server::http(("0.0.0.0", port))
        .map_err(|e| format!("couldn't bind port {port}: {e}"))?;

    let cards_path = format!("{CARDS_DIR}/cards-{}.jsonl", crate::campaign::today());
    let processed = read_lossy(Path::new(&cards_path))
        .map(|text| text.lines().filter(|line| !line.trim().is_empty()).count())
        .unwrap_or(0);
    let state = Arc::new(Mutex::new(State { processed, ..State::default() }));
    spawn_poller(Arc::clone(&state));
    let players = Arc::new(build_players()?);

    let base = lan_url(port);
    println!("magehand table server\n");
    println!("  DM dashboard (shows secrets):  {base}/?t={token}");
    println!("  player join page (QR codes):   {base}/join?t={token}");
    println!(
        "\n{} player(s) rostered. Open the join page on the DM laptop and let players scan.",
        players.by_token.len()
    );
    println!("run `magehand listen` alongside this to feed the card feed. Ctrl-C to stop.");

    for request in server.incoming_requests() {
        let method = request.method().clone();
        let url = request.url().to_string();
        let path = url.split('?').next().unwrap_or("/").to_string();

        // --- player routes (own capability cookie, scoped to one player) ---
        if let Some(seg) = path.strip_prefix("/p/") {
            route_player(request, &method, seg, &url, &players, &state);
            continue;
        }

        // --- DM routes (require the DM token) ---
        if !authed(&request, &token) {
            if method == Method::Get && path == "/" && query_token(&url).as_deref() == Some(&token) {
                let _ = request.respond(
                    Response::from_string(DM_HTML).with_header(html_hdr()).with_header(cookie_hdr("mh", &token)),
                );
            } else if method == Method::Get && path == "/join" && query_token(&url).as_deref() == Some(&token) {
                let _ = request.respond(join_page(&players, &base).with_header(html_hdr()));
            } else {
                let _ = request.respond(Response::from_string("unauthorized").with_status_code(401));
            }
            continue;
        }

        match (&method, path.as_str()) {
            (Method::Get, "/") => {
                let _ = request.respond(Response::from_string(DM_HTML).with_header(html_hdr()));
            }
            (Method::Get, "/join") => {
                let _ = request.respond(join_page(&players, &base).with_header(html_hdr()));
            }
            (Method::Get, "/events") => {
                let st = Arc::clone(&state);
                std::thread::spawn(move || stream_events(request, st));
            }
            (Method::Post, "/action") => {
                // off the accept loop: a slow/held-open body must not freeze the
                // whole dashboard for every other client
                let st = Arc::clone(&state);
                std::thread::spawn(move || handle_action(request, st));
            }
            _ => {
                let _ = request.respond(Response::from_string("not found").with_status_code(404));
            }
        }
    }
    Ok(())
}

// ---------- player surface ----------

fn build_players() -> Result<Players> {
    let mut by_token = HashMap::new();
    for slug in sheets::roster() {
        let token = mint_token()?;
        by_token.insert(
            token.clone(),
            PInfo { slug: slug.clone(), name: sheets::display_name(&slug), token },
        );
    }
    Ok(Players { by_token, ask_log: Mutex::new(HashMap::new()) })
}

fn route_player(
    request: tiny_http::Request,
    method: &Method,
    seg: &str,
    url: &str,
    players: &Arc<Players>,
    _state: &Arc<Mutex<State>>,
) {
    match (method, seg) {
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
        // anything else under /p/ is a capability-token landing
        (Method::Get, tok) => {
            let tok = tok.split('?').next().unwrap_or(tok);
            match players.by_token.get(tok) {
                Some(_) => {
                    let _ = request.respond(
                        Response::from_string(PLAYER_HTML)
                            .with_header(html_hdr())
                            .with_header(cookie_hdr("mhp", tok)),
                    );
                }
                None => {
                    let _ = request.respond(Response::from_string("unknown player link").with_status_code(404));
                }
            }
        }
        _ => reject(request),
    }
    let _ = url; // reserved for future query handling
}

fn player_auth(request: &tiny_http::Request, players: &Players) -> Option<PInfo> {
    let tok = cookie_of(request, "mhp")?;
    players.by_token.get(&tok).cloned()
}

fn player_data(p: &PInfo) -> Value {
    let (fields, body) = sheets::read_sheet(&p.slug)
        .map(|(f, b)| (f, b))
        .unwrap_or_else(|| (Vec::new(), String::new()));
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
        "has_sheet": !fields.is_empty(),
        "fields": fields,
        "body": body,
        "secrets": sheets::secrets_of(&p.slug),
        "recap": sheets::latest_recap(),
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

/// DM-shown page: one QR per player linking to their capability URL.
fn join_page(players: &Players, base: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut cards = String::new();
    let mut sorted: Vec<&PInfo> = players.by_token.values().collect();
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
        // threads snapshot + transcript tail (cheap file reads; replace on change)
        let threads = read_threads();
        let transcript = read_transcript();
        {
            let mut st = state.lock().unwrap();
            if threads != st.threads_json {
                st.threads_json = threads;
            }
            if transcript != st.transcript {
                st.transcript = transcript;
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
    let mut sent_threads = String::new();
    let mut sent_transcript = String::new();
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
            if st.threads_json != sent_threads && !st.threads_json.is_empty() {
                sent_threads = st.threads_json.clone();
                out.push_str(&sse("threads", &sent_threads));
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

fn cookie_hdr(name: &str, token: &str) -> Header {
    // session cookie, host-only; SameSite=Lax so the token in the URL sets it on first load
    header("Set-Cookie", &format!("{name}={token}; Path=/; SameSite=Lax"))
}
