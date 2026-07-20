use crate::campaign::{ensure_vault, last_session_number, md_files, one_shot, CAMPAIGN};
use crate::{context_block, ingest, open_db, read_lossy, search, strip_frontmatter, Result};
use std::path::Path;

const CAL: &str = "sources/campaign/calendar.md";
const LOOT: &str = "sources/campaign/loot.md";
const DOWNTIME: &str = "sources/campaign/downtime.md";

// ---------- calendar ----------

/// The day counter is the source of truth; named dates are derived from an
/// optional `months:` line so any homebrew calendar works without code.
pub(crate) fn cmd_time(args: &[String]) -> Result<()> {
    ensure_vault()?;
    ensure_calendar()?;
    let text = read_lossy(Path::new(CAL))?;
    let day = current_day_of(&text).ok_or("calendar.md has no numeric `day:` in its frontmatter")?;
    match args.first().map(String::as_str) {
        None => {
            println!("{}", format_day(&text, day));
            Ok(())
        }
        Some("advance") if args.len() == 2 => {
            let n: i64 = args[1]
                .trim_end_matches('d')
                .parse()
                .map_err(|_| "advance takes a number of days, e.g. `3` or `3d`")?;
            let new_day = day.checked_add(n).ok_or("day counter overflow")?;
            set_day(new_day)?;
            let text = read_lossy(Path::new(CAL))?;
            println!("{}", format_day(&text, new_day));
            due_check(new_day)?;
            ingest()
        }
        Some("set") if args.len() == 2 => {
            let n: i64 = args[1].parse().map_err(|_| "set takes an absolute day number")?;
            set_day(n)?;
            let text = read_lossy(Path::new(CAL))?;
            println!("{}", format_day(&text, n));
            due_check(n)?;
            ingest()
        }
        _ => Err("usage: magehand time [advance <days> | set <day>]".into()),
    }
}

/// Campaign chronology from session frontmatter — no LLM.
pub(crate) fn cmd_timeline() -> Result<()> {
    ensure_vault()?;
    let sessions = md_files(&format!("{CAMPAIGN}/sessions"));
    if sessions.is_empty() {
        return Err("no sessions logged yet — archive one with `magehand log`".into());
    }
    for p in sessions {
        // one unreadable entry must not hide the rest of the chronology
        let text = match read_lossy(&p) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("skipping {}: {e}", p.display());
                continue;
            }
        };
        let session = fm_value(&text, "session").unwrap_or_else(|| "?".into());
        let date = fm_value(&text, "date").unwrap_or_default();
        let day = fm_value(&text, "day").map(|d| format!("day {d}, ")).unwrap_or_default();
        let summary: String = text
            .split("## Summary")
            .nth(1)
            .and_then(|s| s.lines().find(|l| !l.trim().is_empty()))
            .unwrap_or("")
            .chars()
            .take(120)
            .collect();
        println!("s{session:>02} ({day}{date}): {}", summary.trim());
    }
    Ok(())
}

/// In-world day if a calendar exists — used to stamp session logs and ledgers.
pub(crate) fn current_day() -> Option<i64> {
    current_day_of(&read_lossy(Path::new(CAL)).ok()?)
}

fn current_day_of(text: &str) -> Option<i64> {
    fm_value(text, "day")?.parse().ok()
}

fn ensure_calendar() -> Result<()> {
    if !Path::new(CAL).exists() {
        std::fs::write(
            CAL,
            "---\nkind: calendar\nday: 1\n---\n\n# Calendar\n\n\
             The frontmatter `day:` counter is the source of truth — advance it with\n\
             `magehand time advance 3`. For named dates, add (unindented) lines like:\n\n\
             <!--\n    months: Hammer:30, Alturiak:30, Ches:30, Tarsakh:30\n    year: 1491\n-->\n",
        )?;
    }
    Ok(())
}

fn prepare_day(n: i64) -> Result<String> {
    let text = read_lossy(Path::new(CAL))?;
    set_fm_line(&text, "day", &n.to_string())
        .ok_or_else(|| "calendar.md frontmatter is malformed — no `day:` line to update".into())
}

fn set_day(n: i64) -> Result<()> {
    std::fs::write(CAL, prepare_day(n)?)?;
    Ok(())
}

fn format_day(text: &str, day: i64) -> String {
    let months = parse_months(text);
    if months.is_empty() {
        return format!("Day {day}");
    }
    let year_len: i64 = months.iter().map(|(_, l)| l).sum();
    let base_year: i64 = body_value(text, "year").and_then(|v| v.parse().ok()).unwrap_or(1);
    let d0 = day - 1;
    let year = base_year + d0.div_euclid(year_len);
    let mut doy = d0.rem_euclid(year_len);
    for (name, len) in &months {
        if doy < *len {
            return format!("{} {name}, {year} (day {day})", doy + 1);
        }
        doy -= len;
    }
    format!("Day {day}")
}

fn parse_months(text: &str) -> Vec<(String, i64)> {
    body_value(text, "months")
        .map(|spec| {
            spec.split(',')
                .filter_map(|m| {
                    let (name, len) = m.trim().rsplit_once(':')?;
                    let len: i64 = len.trim().parse().ok()?;
                    (len > 0 && !name.trim().is_empty()).then(|| (name.trim().to_string(), len))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Open threads whose `due:` day has arrived surface at every time advance.
fn due_check(day: i64) -> Result<()> {
    for p in md_files(&format!("{CAMPAIGN}/threads")) {
        let t = read_lossy(&p)?;
        if fm_value(&t, "status").as_deref() != Some("open") {
            continue;
        }
        let Some(due) = fm_value(&t, "due").and_then(|v| v.parse::<i64>().ok()) else {
            continue;
        };
        if due <= day {
            let title = t.lines().find_map(|l| l.strip_prefix("# ")).unwrap_or("?");
            println!("⏰ due (day {due}): {title}");
        }
    }
    Ok(())
}

// ---------- loot ----------

pub(crate) fn cmd_loot(args: &[String]) -> Result<()> {
    ensure_vault()?;
    ensure_loot()?;
    match args.first().map(String::as_str) {
        None | Some("list") => {
            let text = read_lossy(Path::new(LOOT))?;
            println!("{}", strip_frontmatter(&text).trim());
            println!("\nParty fund: {}", fund_display(&text));
            Ok(())
        }
        Some("add") if args.len() >= 3 => {
            let mut rest: Vec<String> = args[1..].to_vec();
            let secret = match rest.iter().position(|a| a == "--secret") {
                Some(i) => {
                    let s = rest[i + 1..].join(" ");
                    rest.truncate(i);
                    if s.is_empty() {
                        return Err("--secret needs the true identity text".into());
                    }
                    format!(" ⟂ DM: {s}")
                }
                None => String::new(),
            };
            if rest.len() < 2 {
                return Err("usage: magehand loot add <holder> <item…> [--secret <truth>]".into());
            }
            let (holder, item) = (&rest[0], rest[1..].join(" "));
            let mut text = read_lossy(Path::new(LOOT))?;
            text.push_str(&format!("\n- ({}) [{holder}] {item}{secret}\n", provenance()));
            std::fs::write(LOOT, text)?;
            println!("recorded → {LOOT}");
            ingest()
        }
        Some("fund") if args.len() >= 2 => {
            let delta = parse_coins(&args[1])?;
            let note = args[2..].join(" ");
            let total = apply_fund(delta, if note.is_empty() { "adjustment" } else { &note })?;
            println!("fund is now {total}");
            ingest()
        }
        _ => Err("usage: magehand loot <list | add <holder> <item…> [--secret <truth>] | fund <±amount> [note]>".into()),
    }
}

fn ensure_loot() -> Result<()> {
    if !Path::new(LOOT).exists() {
        std::fs::write(LOOT, "---\nkind: loot\nfund_cp: 0\n---\n\n# Party loot\n")?;
    }
    Ok(())
}

/// Money reads for MUTATION must be strict: a hand-edited `fund_cp: 12,500`
/// silently becoming 0 would destroy the balance on the next write.
fn fund_cp_strict(text: &str) -> Result<i64> {
    fm_value(text, "fund_cp")
        .ok_or("loot.md has no `fund_cp:` line in its frontmatter")?
        .parse()
        .map_err(|_| "loot.md `fund_cp:` isn't a plain integer — fix it before touching the fund".into())
}

fn fund_display(text: &str) -> String {
    match fund_cp_strict(text) {
        Ok(cp) => fmt_cp(cp),
        Err(e) => format!("unreadable ({e})"),
    }
}

/// Validate + build the updated loot file without writing (so callers can
/// stage all mutations before committing any). Returns (new text, new total).
fn prepare_fund(delta_cp: i64, note: &str) -> Result<(String, String)> {
    ensure_loot()?;
    let text = read_lossy(Path::new(LOOT))?;
    let total = fund_cp_strict(&text)?
        .checked_add(delta_cp)
        .ok_or("fund arithmetic overflow")?;
    let mut updated = set_fm_line(&text, "fund_cp", &total.to_string())
        .ok_or("loot.md frontmatter is malformed — no `fund_cp:` line to update")?;
    let sign = if delta_cp >= 0 { "+" } else { "-" };
    updated.push_str(&format!(
        "\n- ({}) fund {sign}{} — {note} (total {})\n",
        provenance(),
        fmt_cp(delta_cp.abs()),
        fmt_cp(total)
    ));
    Ok((updated, fmt_cp(total)))
}

fn apply_fund(delta_cp: i64, note: &str) -> Result<String> {
    let (updated, total) = prepare_fund(delta_cp, note)?;
    std::fs::write(LOOT, updated)?;
    Ok(total)
}

/// "+150gp", "-3sp", "250 cp", "2,500" (defaults to gp) → copper.
fn parse_coins(s: &str) -> Result<i64> {
    let t = s.trim().replace(',', "");
    let (sign, t) = match t.strip_prefix('-') {
        Some(rest) => (-1i64, rest),
        None => (1, t.strip_prefix('+').unwrap_or(&t)),
    };
    let digits: String = t.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return Err(format!("can't parse amount `{s}` — use e.g. +150gp, -3sp, 25cp").into());
    }
    let n: i64 = digits.parse()?;
    let mult = match t[digits.len()..].trim().to_lowercase().as_str() {
        "gp" | "" => 100,
        "sp" => 10,
        "cp" => 1,
        u => return Err(format!("unknown coin `{u}` — use gp, sp, or cp").into()),
    };
    n.checked_mul(mult)
        .map(|v| sign * v)
        .ok_or_else(|| format!("amount `{s}` is too large").into())
}

/// Pull the leading "±N unit" out of an LLM cost line, dropping trailing prose
/// but never the unit — "250 cp (for supplies)" must stay 250cp, not become 250gp.
fn leading_amount(s: &str) -> String {
    let s = s.trim();
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    if matches!(chars.peek(), Some('+' | '-')) {
        out.push(chars.next().unwrap());
    }
    while matches!(chars.peek(), Some(c) if c.is_ascii_digit() || *c == ',') {
        out.push(chars.next().unwrap());
    }
    while chars.peek() == Some(&' ') {
        chars.next();
    }
    while matches!(chars.peek(), Some(c) if c.is_ascii_alphabetic()) {
        out.push(chars.next().unwrap());
    }
    out
}

fn fmt_cp(cp: i64) -> String {
    let sign = if cp < 0 { "-" } else { "" };
    let cp = cp.abs();
    let mut parts = Vec::new();
    if cp / 100 > 0 {
        parts.push(format!("{}gp", cp / 100));
    }
    if (cp % 100) / 10 > 0 {
        parts.push(format!("{}sp", (cp % 100) / 10));
    }
    if cp % 10 > 0 || parts.is_empty() {
        parts.push(format!("{}cp", cp % 10));
    }
    format!("{sign}{}", parts.join(" "))
}

fn provenance() -> String {
    let day = current_day().map(|d| format!("day {d}, ")).unwrap_or_default();
    format!("{day}s{}", last_session_number())
}

// ---------- downtime ----------

pub(crate) fn cmd_downtime(args: &[String]) -> Result<()> {
    let commit = args.iter().any(|a| a == "--commit");
    let activity: String = args
        .iter()
        .filter(|a| a.as_str() != "--commit")
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    if activity.is_empty() {
        return Err("usage: magehand downtime <who does what for how long> [--commit]".into());
    }
    ensure_vault()?; // before the paid LLM call
    ensure_loot()?;
    ensure_calendar()?;
    let conn = open_db()?;
    let rules = context_block(&search(&conn, &activity, 8, Some("rules"), false)?);
    let canon = context_block(&search(&conn, &activity, 4, Some("campaign"), false)?);
    let day = current_day().unwrap_or(1);
    let fund = fund_display(&read_lossy(Path::new(LOOT))?);
    let prompt = format!(
        "Resolve a tabletop RPG downtime activity strictly by the rules in the excerpts.\n\
         Activity: {activity}\nCurrent in-world day: {day}. Party fund: {fund}.\n\n\
         Rulebook excerpts:\n{rules}\n\nCampaign context:\n{canon}\n\n\
         Output '## Resolution' — the governing rule with its citation, the checks involved as \
         suggested rolls the DM can roll or accept, costs, and complications. If the excerpts \
         have no applicable rule, say so and improvise a fair one, marked (improvised).\n\
         Then end with exactly three final lines:\n\
         DAYS: <integer days consumed>\nCOST: <total cost like 25gp, or 0gp>\n\
         OUTCOME: <one line to record in the campaign ledger>"
    );
    let out = one_shot(&prompt)?;
    println!("{out}");
    if commit {
        // A commit touches money, time, and the canon ledger. Parse everything
        // strictly, stage every mutation, and only then write — with the ledger
        // claim LAST, so downtime.md never records a charge that didn't happen.
        let days: i64 = label_value(&out, "DAYS")
            .and_then(|v| v.trim_end_matches('d').trim().parse().ok())
            .ok_or("couldn't parse DAYS — nothing committed")?;
        if days < 0 {
            return Err("model reported negative DAYS — nothing committed".into());
        }
        let cost_line = label_value(&out, "COST").ok_or("couldn't parse COST — nothing committed")?;
        let cost_cp = parse_coins(&leading_amount(&cost_line))
            .map_err(|e| format!("couldn't parse COST `{cost_line}` — nothing committed ({e})"))?
            .abs();
        let outcome = label_value(&out, "OUTCOME").ok_or("couldn't parse OUTCOME — nothing committed")?;

        let fund_staged = if cost_cp > 0 {
            Some(prepare_fund(-cost_cp, &format!("downtime: {activity}"))?)
        } else {
            None
        };
        let cal_staged = if days > 0 { Some(prepare_day(day + days)?) } else { None };
        if !Path::new(DOWNTIME).exists() {
            std::fs::write(DOWNTIME, "---\nkind: downtime\n---\n\n# Downtime log\n")?;
        }
        let mut log = read_lossy(Path::new(DOWNTIME))?;
        log.push_str(&format!(
            "\n- (day {day}–{}, s{}) {activity}: {outcome} ({}d, cost {})\n",
            day + days,
            last_session_number(),
            days,
            fmt_cp(cost_cp)
        ));

        if let Some((text, _)) = &fund_staged {
            std::fs::write(LOOT, text)?;
        }
        if let Some(text) = &cal_staged {
            std::fs::write(CAL, text)?;
        }
        std::fs::write(DOWNTIME, log)?;
        println!("\n→ committed: day {} → {}, fund charged {}, logged in {DOWNTIME}", day, day + days, fmt_cp(cost_cp));
        due_check(day + days)?;
        ingest()?;
    }
    Ok(())
}

/// Tolerant "LABEL: value" extraction — models bold labels and vary case.
fn label_value(out: &str, label: &str) -> Option<String> {
    out.lines().rev().find_map(|l| {
        let clean = l.trim().trim_matches(|c| c == '*' || c == '_');
        let p = clean.get(..label.len() + 1)?;
        if p.eq_ignore_ascii_case(&format!("{label}:")) {
            Some(
                clean[label.len() + 1..]
                    .trim_matches(|c: char| c == '*' || c == '_' || c.is_whitespace())
                    .to_string(),
            )
        } else {
            None
        }
    })
}

// ---------- shared frontmatter helpers ----------

/// Replace `key: …` inside the leading frontmatter block only.
fn set_fm_line(text: &str, key: &str, value: &str) -> Option<String> {
    let mut lines = text.split_inclusive('\n');
    let first = lines.next()?;
    if first.trim_end() != "---" {
        return None;
    }
    let mut out = String::with_capacity(text.len() + 16);
    out.push_str(first);
    let mut in_frontmatter = true;
    let mut replaced = false;
    for line in lines {
        if in_frontmatter && !replaced && line.trim_start().starts_with(&format!("{key}:")) {
            out.push_str(&format!("{key}: {value}\n"));
            replaced = true;
            continue;
        }
        if in_frontmatter && line.trim_end() == "---" {
            in_frontmatter = false;
        }
        out.push_str(line);
    }
    replaced.then_some(out)
}

pub(crate) fn fm_value(text: &str, key: &str) -> Option<String> {
    let mut lines = text.lines();
    if lines.next()?.trim_end() != "---" {
        return None;
    }
    for line in lines {
        if line.trim_end() == "---" {
            break;
        }
        if let Some(v) = line.strip_prefix(&format!("{key}:")) {
            return Some(v.trim().to_string());
        }
    }
    None
}

/// `key: value` lines in the body (below frontmatter), for calendar config.
fn body_value(text: &str, key: &str) -> Option<String> {
    strip_frontmatter(text)
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{key}:")))
        .map(|v| v.trim().to_string())
}
