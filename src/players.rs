use crate::campaign::{ensure_vault, last_session_number, md_files, one_shot, slugify, thread_lines, today, CAMPAIGN};
use crate::{context_block, open_db, read_lossy, search, Result};
use std::path::Path;

/// Cross-search next session's material against player backstories: one tie-in
/// per character. Backstories live in sources/campaign/backstories/<character>.md.
pub(crate) fn cmd_hooks(topic: &str) -> Result<()> {
    ensure_vault()?; // before the paid LLM call
    let stories = read_dir_notes(&format!("{CAMPAIGN}/backstories"))?;
    if stories.is_empty() {
        return Err(format!(
            "no backstories found — drop one markdown file per character into {CAMPAIGN}/backstories/"
        )
        .into());
    }
    let conn = open_db()?;
    let ctx = context_block(&search(&conn, topic, 10, None, false)?);
    let prompt = format!(
        "Next session's material for a tabletop RPG campaign: {topic}\n\n\
         Source excerpts:\n{ctx}\n\nPlayer character backstories:\n{stories}\n\n\
         For EACH character, propose exactly one concrete tie-in between their backstory and this \
         material — an NPC who knew someone, a place they've been, a debt resurfacing. Quote the \
         backstory line it hangs on. If a character genuinely has no plausible tie, say so rather \
         than forcing one. Format: ### <Character> / one short paragraph."
    );
    println!("{}", one_shot(&prompt)?);
    Ok(())
}

/// Who-knows-what ledger: one DM-only file per player under campaign/secrets/.
pub(crate) fn cmd_secret(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("add") if args.len() >= 3 => {
            ensure_vault()?;
            let slug = slugify(&args[1]);
            if slug.is_empty() {
                return Err("player name needs at least one letter or number".into());
            }
            let path = format!("{CAMPAIGN}/secrets/{slug}.md");
            let mut cur = read_lossy(Path::new(&path))
                .unwrap_or_else(|_| format!("# Secrets — {}\n", args[1]));
            let text = args[2..].join(" ");
            let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
            if one_line.is_empty() {
                return Err("secret text is empty (unset shell variable?)".into());
            }
            cur.push_str(&format!("\n- {} (session {}): {}\n", today(), last_session_number(), one_line));
            std::fs::write(&path, cur)?;
            println!("recorded secret for {} → {path}", args[1]);
            crate::ingest()
        }
        Some("list") => {
            let filter = args.get(1).map(|p| slugify(p));
            let mut shown = 0;
            for p in md_files(&format!("{CAMPAIGN}/secrets")) {
                let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                if filter.as_deref().is_some_and(|f| f != stem) {
                    continue;
                }
                println!("{}\n", read_lossy(&p)?.trim());
                shown += 1;
            }
            if shown == 0 {
                println!("no secrets recorded{}", filter.map(|f| format!(" for `{f}`")).unwrap_or_default());
            }
            Ok(())
        }
        _ => Err("usage: magehand secret <add <player> <text> | list [player]>".into()),
    }
}

/// "What you missed" brief for an absent player — built only from session records
/// and that player's own secrets, so other players' secrets can't leak into it.
pub(crate) fn cmd_catchup(args: &[String]) -> Result<()> {
    let Some(player) = args.first() else {
        return Err("usage: magehand catchup <player> [sessions-missed]".into());
    };
    ensure_vault()?; // before the paid LLM call
    let missed: usize = match args.get(1) {
        None => 2,
        // a typo'd count must not silently become a different brief
        Some(n) => n.parse().map_err(|_| format!("sessions-missed must be a number, got `{n}`"))?,
    };
    if missed == 0 {
        return Err("sessions-missed must be at least 1".into());
    }
    let sessions = md_files(&format!("{CAMPAIGN}/sessions"));
    if sessions.is_empty() {
        return Err("no sessions logged yet — archive one with `magehand log`".into());
    }
    let recent = sessions
        .iter()
        .rev()
        .take(missed)
        .rev()
        .map(|p| Ok(format!("--- {} ---\n{}", p.display(), read_lossy(p)?)))
        .collect::<Result<Vec<_>>>()?
        .join("\n\n");
    let own_secrets = read_lossy(Path::new(&format!("{CAMPAIGN}/secrets/{}.md", slugify(player))))
        .unwrap_or_else(|_| "none".into());
    let prompt = format!(
        "Write a catch-up brief for {player}, a tabletop RPG player who missed the session(s) below. \
         Second person, one page max, ending with where the party stands right now.\n\
         Include ONLY what happened openly at the table or is listed under 'Revealed to players'. \
         Skip DM-only sections (Contradiction check, Suggested threads) entirely.\n\n\
         Session records:\n{recent}\n\n\
         Things only {player} knows (weave in as reminders where relevant):\n{own_secrets}"
    );
    println!("(review before sending — this brief is player-facing)\n");
    println!("{}", one_shot(&prompt)?);
    Ok(())
}

/// One-page primer for a new player, built from player-visible material only.
pub(crate) fn cmd_onboard() -> Result<()> {
    ensure_vault()?; // before the paid LLM call
    let recaps = read_dir_notes(&format!("{CAMPAIGN}/recaps"))?;
    if recaps.is_empty() {
        return Err("no recaps yet — run `magehand recap` after logging a session".into());
    }
    let house = read_lossy(Path::new("sources/house/houserules.md")).unwrap_or_else(|_| "none".into());
    let rulings = read_lossy(Path::new("sources/house/rulings.md")).unwrap_or_else(|_| "none".into());
    let prompt = format!(
        "Write a one-page onboarding primer for a NEW PLAYER joining an ongoing tabletop RPG \
         campaign. Sections: The story so far (short) / People you'll hear about / Where things \
         stand / Table rules. Friendly, spoiler-free, no DM commentary.\n\n\
         Player-safe recaps:\n{recaps}\n\nHouse rules:\n{house}\n\nTable rulings:\n{rulings}"
    );
    println!("{}", one_shot(&prompt)?);
    Ok(())
}

/// Who hasn't had a scene lately, and what to give them.
pub(crate) fn cmd_spotlight() -> Result<()> {
    ensure_vault()?; // before the paid LLM call
    let sessions = md_files(&format!("{CAMPAIGN}/sessions"));
    if sessions.is_empty() {
        return Err("no sessions logged yet — archive one with `magehand log`".into());
    }
    let recent = sessions
        .iter()
        .rev()
        .take(3)
        .rev()
        .map(|p| Ok(format!("--- {} ---\n{}", p.display(), read_lossy(p)?)))
        .collect::<Result<Vec<_>>>()?
        .join("\n\n");
    // propagate read errors like hooks does — a permission failure is not "no backstories"
    let stories = read_dir_notes(&format!("{CAMPAIGN}/backstories"))?;
    let threads = thread_lines(true)?.join("\n");
    let prompt = format!(
        "Analyze spotlight distribution across player characters in these tabletop RPG session \
         records.\n\nRecent sessions:\n{recent}\n\nBackstories (may be empty):\n{stories}\n\n\
         Open threads:\n{threads}\n\n\
         Output: ### Spotlight — per character, how present/agentive they were (cite moments); \
         ### Neglected — who's been quiet and for how long; ### Suggested scenes — one concrete \
         scene per neglected character, tied to their backstory or an open thread."
    );
    println!("{}", one_shot(&prompt)?);
    Ok(())
}

fn read_dir_notes(dir: &str) -> Result<String> {
    md_files(dir)
        .iter()
        .map(|p| Ok(format!("--- {} ---\n{}", p.display(), read_lossy(p)?)))
        .collect::<Result<Vec<_>>>()
        .map(|v| v.join("\n\n"))
}
