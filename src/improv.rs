use crate::campaign::{ensure_vault, last_session_number, one_shot, slugify, thread_lines, today, CAMPAIGN};
use crate::{context_block, open_db, read_lossy, search, Result};
use std::path::Path;

/// "Is there a thieves' guild here?" — answer YES, elaborated consistently with
/// canon, contradictions flagged. --commit makes the improvised fact canon.
pub(crate) fn cmd_yesand(args: &[String]) -> Result<()> {
    let commit = args.iter().any(|a| a == "--commit");
    let question: String = args
        .iter()
        .filter(|a| a.as_str() != "--commit")
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    if question.is_empty() {
        return Err("usage: magehand yesand <in-world question> [--commit]".into());
    }
    ensure_vault()?; // guard BEFORE the paid LLM call — wrong cwd must not cost money
    if crate::fts_query(&question).is_empty() {
        return Err("question has no searchable terms".into());
    }
    let conn = open_db()?;
    let ctx = context_block(&search(&conn, &question, 10, None, false)?);
    let prompt = format!(
        "A tabletop RPG DM wants to say YES to an in-world question mid-session without \
         breaking canon.\nQuestion: {question}\n\nEstablished canon and setting excerpts:\n{ctx}\n\n\
         Answer YES with a concrete elaboration that fits every excerpt. Name what constrains it \
         (cite [source — title]). If a flat yes would contradict canon, lead with 'CONFLICT:' and \
         explain, then offer the closest yes that works. End with one line starting exactly \
         'CANON: ' — a single sentence stating the new fact worth recording."
    );
    let out = one_shot(&prompt)?;
    println!("{out}");
    if commit {
        // tolerate '**CANON:**' etc. — models bold labels, and the reply is paid for
        let Some(fact) = out.lines().rev().find_map(|l| {
            l.trim()
                .trim_matches(|c| c == '*' || c == '_')
                .strip_prefix("CANON:")
                .map(|f| f.trim_matches(|c: char| c == '*' || c == '_' || c.is_whitespace()).to_string())
        }) else {
            return Err("no CANON: line in the reply — nothing committed".into());
        };
        let path = format!("{CAMPAIGN}/improv.md");
        let mut cur = read_lossy(Path::new(&path)).unwrap_or_else(|_| "# Improvised canon\n".into());
        cur.push_str(&format!("\n- {} (session {}): {}\n", today(), last_session_number(), fact.trim()));
        std::fs::write(&path, cur)?;
        println!("\n→ committed to {path}");
        crate::ingest()?;
    }
    Ok(())
}

/// Off-script ripple: immediate fallout now, delayed consequences saved as
/// trigger-tagged threads so prep/recap resurface them automatically.
pub(crate) fn cmd_consequence(args: &[String]) -> Result<()> {
    let save = args.iter().any(|a| a == "--save");
    let event: String = args
        .iter()
        .filter(|a| a.as_str() != "--save")
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    if event.is_empty() {
        return Err("usage: magehand consequence <what the party just did> [--save]".into());
    }
    ensure_vault()?; // guard BEFORE the paid LLM call
    let conn = open_db()?;
    let ctx = context_block(&search(&conn, &event, 10, Some("campaign"), false)?);
    let threads = thread_lines(true)?.join("\n");
    let prompt = format!(
        "The party just did something off-script in a tabletop RPG campaign:\n{event}\n\n\
         Established canon:\n{ctx}\n\nOpen threads:\n{threads}\n\n\
         Ground every consequence in the involved factions/NPCs from the excerpts. Output exactly:\n\
         ## Immediate\n(2-3 bullets: fallout the DM narrates in the next scene)\n\
         ## Delayed\n(2-3 bullets, each formatted exactly as:\n\
         - <short title> | trigger: <observable condition that fires it> | <what happens>)"
    );
    let out = one_shot(&prompt)?;
    println!("{out}");
    if save {
        let mut saved = 0;
        let delayed = out.split("## Delayed").nth(1).unwrap_or("");
        for line in delayed.lines().filter_map(|l| l.trim().strip_prefix("- ")) {
            // title | trigger | detail — splitn keeps any further pipes inside the detail
            let mut parts = line.splitn(3, '|').map(str::trim);
            let (Some(raw_title), Some(raw_trigger)) = (parts.next(), parts.next()) else {
                continue;
            };
            if raw_trigger.is_empty() {
                continue;
            }
            // models like to bold labels — markdown markers don't belong in either field
            let title = raw_title.trim_matches(|c| c == '*' || c == '_' || c == ' ');
            let slug = slugify(title);
            if slug.is_empty() {
                eprintln!("skipping a delayed consequence with no usable title");
                continue;
            }
            let t = raw_trigger.trim_matches(|c| c == '*' || c == '_').trim();
            let trigger = match t.get(..8) {
                // case-tolerant "trigger:" prefix strip; .get() can't panic mid-codepoint
                Some(p) if p.eq_ignore_ascii_case("trigger:") => t[8..].trim(),
                _ => t,
            };
            let detail = parts.next().unwrap_or("");
            let date = today();
            let path = crate::campaign::free_path(&format!("{CAMPAIGN}/threads"), &slug);
            std::fs::write(
                &path,
                format!(
                    "---\nkind: consequence\nstatus: open\nopened: {date}\ntrigger: {trigger}\n---\n\n\
                     # {title}\n\nPending consequence of: {event}\nTrigger: {trigger}\n{detail}\n"
                ),
            )?;
            saved += 1;
        }
        if saved == 0 {
            return Err("couldn't parse any delayed consequences — nothing saved (re-run, or add them with `magehand thread add`)".into());
        }
        println!("\n→ saved {saved} pending consequence(s) as threads");
        crate::ingest()?;
    }
    Ok(())
}

/// Read-aloud text that only uses details actually present in your sources.
pub(crate) fn cmd_boxtext(scene: &str) -> Result<()> {
    ensure_vault()?;
    if crate::fts_query(scene).is_empty() {
        return Err("scene has no searchable terms — nothing to ground the text in".into());
    }
    let conn = open_db()?;
    let ctx = context_block(&search(&conn, scene, 10, None, false)?);
    let prompt = format!(
        "Write boxed read-aloud text for a tabletop RPG scene: {scene}\n\n\
         Source excerpts:\n{ctx}\n\n\
         3-5 sentences, second person, present tense, sensory but not purple. Use ONLY details \
         present in the excerpts — invent atmosphere, never facts. After the text, list the \
         citations used as bullets ([source — title])."
    );
    println!("{}", one_shot(&prompt)?);
    Ok(())
}

/// `statblock <name>` condenses a monster from your books into a play-priority
/// crib; `--stub <desc>` drafts a homebrew block from SRD comparables; `--save`
/// canonizes either into the vault.
pub(crate) fn cmd_statblock(args: &[String]) -> Result<()> {
    let stub = args.iter().any(|a| a == "--stub");
    let save = args.iter().any(|a| a == "--save");
    let query: String = args
        .iter()
        .filter(|a| !a.starts_with("--"))
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    if query.is_empty() || crate::fts_query(&query).is_empty() {
        return Err("usage: magehand statblock <monster name> [--save] | --stub <concept, e.g. 'CR 3 undead archer'> [--save]".into());
    }
    ensure_vault()?; // before the paid LLM call
    let conn = open_db()?;
    let hits = search(&conn, &query, 6, None, false)?;
    let ctx = context_block(&hits);
    let prompt = if stub {
        format!(
            "Draft ONE complete tabletop RPG stat block for: {query}\n\n\
             Comparable creatures from the user's books (derive sane numbers — AC, HP, attack \
             bonus, damage, save DCs — from these, matching the target CR's math):\n{ctx}\n\n\
             Output markdown starting exactly with '# <Name> (CR <x>)', then the standard stat \
             block sections (type/alignment, AC, HP, Speed, ability line, skills/senses, traits, \
             actions). Note which comparable each number was derived from at the end."
        )
    } else {
        // OR-retrieval matches on filler words ("the") — require the name's most
        // distinctive term to actually appear in a top hit before paying for a crib
        let anchor = query
            .split_whitespace()
            .max_by_key(|w| w.len())
            .unwrap_or("")
            .to_lowercase();
        let anchored = hits.iter().take(3).any(|h| {
            h.title.to_lowercase().contains(&anchor) || h.body.to_lowercase().contains(&anchor)
        });
        if hits.is_empty() || !anchored {
            return Err(format!("nothing matching `{query}` in your books — did you mean --stub?").into());
        }
        format!(
            "Condense this creature into a one-screen PLAY CRIB a DM can run a turn from.\n\
             Creature: {query}\n\nStat block excerpts:\n{ctx}\n\n\
             Output markdown starting exactly with '# <Name> (CR <x>)', then at most 12 lines: \
             AC/HP/Speed on one line; Opener (best first-round play); On turn (action priority, \
             attack bonuses and damage inline); Reactions/Legendary (with costs); Recharge \
             abilities; Resists/Immunities that matter; Weak to; Source citation. Only use \
             numbers present in the excerpts."
        )
    };
    let out = one_shot(&prompt)?;
    println!("{out}");
    if save {
        let name = out.lines().find_map(|l| l.strip_prefix("# ")).unwrap_or("").trim();
        let slug = slugify(name);
        if slug.is_empty() {
            return Err("generated block has no usable `# Name` line — save it by hand".into());
        }
        let path = crate::campaign::free_path(&format!("{CAMPAIGN}/statblocks"), &slug);
        std::fs::write(&path, format!("---\nkind: statblock\ncreated: {}\n---\n\n{out}\n", today()))?;
        println!("\n→ saved {path}");
        crate::ingest()?;
    }
    Ok(())
}

/// Names that sound like they belong in this setting, deduped against the vault.
pub(crate) fn cmd_name(args: &[String]) -> Result<()> {
    let culture = args.join(" ");
    if culture.is_empty() || crate::fts_query(&culture).is_empty() {
        return Err("usage: magehand name <culture/region/kind, e.g. 'chultan fisherman'>".into());
    }
    ensure_vault()?;
    let conn = open_db()?;
    let ctx = context_block(&search(&conn, &culture, 8, None, false)?);
    let taken: Vec<String> = crate::campaign::md_files(&format!("{CAMPAIGN}/npcs"))
        .iter()
        .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().replace('-', " ")))
        .collect();
    let prompt = format!(
        "Generate 5 character names for: {culture}\n\nSetting excerpts (mimic the naming patterns \
         — phonology, structure — of proper nouns found here):\n{ctx}\n\n\
         Never reuse or closely echo: {}\n\
         Output one name per line, nothing else.",
        if taken.is_empty() { "n/a".to_string() } else { taken.join(", ") }
    );
    println!("{}", one_shot(&prompt)?);
    Ok(())
}
