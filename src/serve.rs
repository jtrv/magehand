use crate::campaign::{cmd_ruling, cmd_thread, ensure_vault, md_files, CAMPAIGN};
use crate::ledger::{current_day, fm_value};
use crate::{read_lossy, Result};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tiny_http::{Header, Method, Response, Server};

const CARDS_DIR: &str = ".magehand/live";
const DM_HTML: &str = include_str!("dm.html");
const POLL: Duration = Duration::from_millis(700);
const MAX_BODY: usize = 16 * 1024;

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

    let state = Arc::new(Mutex::new(State::default()));
    spawn_poller(Arc::clone(&state));

    let base = lan_url(port);
    println!("magehand dashboard (DM-only — shows secrets)\n");
    println!("  open on the DM laptop:  http://localhost:{port}/?t={token}");
    println!("  open on a tablet/LAN:   {base}/?t={token}");
    println!("\nrun `magehand listen` alongside this to feed the card feed. Ctrl-C to stop.");

    for request in server.incoming_requests() {
        let method = request.method().clone();
        let url = request.url().to_string();
        let path = url.split('?').next().unwrap_or("/");

        if !authed(&request, &token) {
            // the landing route may carry the token in the query and set a cookie
            if method == Method::Get && path == "/" && query_token(&url).as_deref() == Some(&token) {
                let _ = request.respond(
                    Response::from_string(DM_HTML)
                        .with_header(html_hdr())
                        .with_header(cookie_hdr(&token)),
                );
            } else {
                let _ = request.respond(Response::from_string("unauthorized").with_status_code(401));
            }
            continue;
        }

        match (&method, path) {
            (Method::Get, "/") => {
                let _ = request.respond(Response::from_string(DM_HTML).with_header(html_hdr()));
            }
            (Method::Get, "/events") => {
                // long-lived SSE stream; own thread so the accept loop stays live
                let st = Arc::clone(&state);
                std::thread::spawn(move || stream_events(request, st));
            }
            (Method::Post, "/action") => {
                let resp = handle_action(request, &state);
                // request already consumed inside handle_action
                let _ = resp;
            }
            _ => {
                let _ = request.respond(Response::from_string("not found").with_status_code(404));
            }
        }
    }
    Ok(())
}

// ---------- shared state ----------

/// Append-only broadcast log of card/dismiss events (bounded to a session's
/// cards), plus replaced-in-place snapshots for threads and the transcript tail.
#[derive(Default)]
struct State {
    log: Vec<Ev>,
    next_id: u64,
    processed: usize, // cards-jsonl lines already turned into Card events
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
                        let id = st.next_id;
                        st.next_id += 1;
                        st.log.push(Ev::Card { id, card });
                        st.processed += 1;
                    }
                    Err(_) => break, // partial trailing write; retry next tick
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
/// card leaves every feed via a Dismiss broadcast once acted.
fn handle_action(mut request: tiny_http::Request, state: &Arc<Mutex<State>>) {
    let mut body = String::new();
    let cap = request.body_length().unwrap_or(0).min(MAX_BODY);
    let _ = request.as_reader().take(cap as u64).read_to_string(&mut body);
    let req: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let action = req["action"].as_str().unwrap_or("");
    let id = req["id"].as_u64();

    // pull the card out under the lock, then run the (slow, re-indexing) command
    // without holding it
    let card = id.and_then(|id| {
        let st = state.lock().unwrap();
        st.log.iter().find_map(|ev| match ev {
            Ev::Card { id: cid, card } if *cid == id => Some(card.clone()),
            _ => None,
        })
    });

    let result: Result<String> = (|| {
        let card = card.ok_or("no such card")?;
        let headline = card["headline"].as_str().unwrap_or("").trim();
        match action {
            "dismiss" => Ok("dismissed".into()),
            "ruling" => {
                let body = card["body"].as_str().unwrap_or("");
                let text = if body.is_empty() {
                    headline.to_string()
                } else {
                    format!("{headline} — {body}")
                };
                if text.trim().is_empty() {
                    return Err("card has no text to record".into());
                }
                cmd_ruling(&text)?;
                Ok("saved as a table ruling".into())
            }
            "thread" => {
                if headline.is_empty() {
                    return Err("card has no title for a thread".into());
                }
                match cmd_thread(&["add".to_string(), headline.to_string()]) {
                    Ok(()) => Ok("opened a thread".into()),
                    // a duplicate title isn't a failure worth blocking the tap on
                    Err(e) if e.to_string().contains("already exists") => Ok("thread already open".into()),
                    Err(e) => Err(e),
                }
            }
            other => Err(format!("unknown action `{other}`").into()),
        }
    })();

    // acted-on cards leave the feed regardless of which action fired
    if let (Some(id), Ok(_)) = (id, &result) {
        let mut st = state.lock().unwrap();
        st.log.push(Ev::Dismiss(id));
    }
    let payload = match result {
        Ok(msg) => json!({ "ok": true, "msg": msg }),
        Err(e) => json!({ "ok": false, "msg": e.to_string() }),
    };
    let _ = request.respond(
        Response::from_string(payload.to_string()).with_header(json_hdr()),
    );
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
    request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Cookie"))
        .map(|h| h.value.as_str())
        .and_then(|c| cookie_val(c, "mh"))
        .is_some_and(|v| v == token)
}

fn cookie_val(cookies: &str, name: &str) -> Option<String> {
    cookies.split(';').find_map(|kv| {
        let (k, v) = kv.trim().split_once('=')?;
        (k == name).then(|| v.to_string())
    })
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

fn cookie_hdr(token: &str) -> Header {
    // session cookie, host-only; SameSite=Lax so the token in the URL sets it on first load
    header("Set-Cookie", &format!("mh={token}; Path=/; SameSite=Lax"))
}
