//! `claudectl hive identity` — show, set or clear this hive's identity (#432).
//!
//! # The `open` join policy, and why it asks
//!
//! RFC Q5 asked whether `join_policy: open` is ever safe, even LAN-only. The
//! ruling taken here is: **allowed, but warranted** — it prints what it actually
//! means, requires a confirmation, and records that the confirmation happened.
//!
//! The recording matters more than the prompt. `identity.json` is an ordinary
//! file an owner can edit, so a prompt alone would be theatre: anyone could
//! write `"join_policy": "open"` by hand and never see the warning. The consent
//! lands in `open_acknowledged_ms`, and
//! [`crate::hive::identity::HiveIdentity::effective_join_policy`] treats an
//! unacknowledged `open` as `invite`. The gate is therefore on the *data*, not
//! on this code path, which is what makes it hold for #433's advertiser too.
//!
//! Without a terminal to prompt on — a script, CI, a provisioning step —
//! `--yes` is required rather than assumed. Defaulting to yes in a pipe is how
//! a permissive setting gets made by accident.

use std::io::{self, IsTerminal};

use crate::hive::identity::{self, HiveIdentity, JoinPolicy};

use super::IdentityAction;

pub fn cmd_identity(action: Option<&IdentityAction>, json_mode: bool) -> io::Result<()> {
    match action {
        None => show(json_mode),
        Some(IdentityAction::Set {
            name,
            description,
            join_policy,
            yes,
        }) => set(
            name.as_deref(),
            description.as_deref(),
            join_policy.map(|p| p.into()),
            *yes,
            json_mode,
        ),
        Some(IdentityAction::Clear { yes }) => clear(*yes, json_mode),
    }
}

fn show(json_mode: bool) -> io::Result<()> {
    let current = identity::load().map_err(io::Error::other)?;

    if json_mode {
        let json = match &current {
            Some(id) => serde_json::json!({
                "named": true,
                "hive_id": id.hive_id,
                "name": id.name,
                "description": id.description,
                "join_policy": id.join_policy.as_str(),
                // The policy that is actually in force, which differs from
                // `join_policy` for an unacknowledged `open`.
                "effective_join_policy": id.effective_join_policy().as_str(),
                "open_acknowledged": id.open_is_acknowledged(),
                "created_ms": id.created_ms,
            }),
            None => serde_json::json!({ "named": false }),
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&json).unwrap_or_default()
        );
        return Ok(());
    }

    let Some(id) = current else {
        println!("This hive is unnamed.");
        println!();
        println!("Nothing is advertised and nothing is discoverable, which is how");
        println!("every install starts. Name it to make it joinable:");
        println!();
        println!("  claudectl hive identity set --name my-hive");
        return Ok(());
    };

    println!("{}  ({})", id.name, id.hive_id);
    if let Some(d) = &id.description {
        println!("{d}");
    }
    println!();
    println!("Join policy:  {}", id.join_policy.as_str());
    if !id.open_is_acknowledged() {
        // The one case where the stored policy is not the real one.
        println!();
        println!("  NOT IN FORCE. This says `open`, but no confirmation was ever");
        println!("  recorded for it — most likely the file was edited by hand.");
        println!(
            "  Treating it as `{}`. To really open the hive:",
            id.effective_join_policy().as_str()
        );
        println!("    claudectl hive identity set --join-policy open");
    } else if id.join_policy == JoinPolicy::Open {
        println!("  Anyone who can see your LAN broadcast may join without approval.");
    }
    println!(
        "Created:      {}",
        claudectl_core::helpers::fmt_ms_at(id.created_ms, identity::epoch_ms())
    );
    Ok(())
}

fn set(
    name: Option<&str>,
    description: Option<&str>,
    join_policy: Option<JoinPolicy>,
    yes: bool,
    json_mode: bool,
) -> io::Result<()> {
    let existing = identity::load().map_err(io::Error::other)?;

    if name.is_none() && description.is_none() && join_policy.is_none() {
        return Err(io::Error::other(
            "nothing to set — pass --name, --description or --join-policy",
        ));
    }
    if existing.is_none() && name.is_none() {
        return Err(io::Error::other(
            "this hive has no name yet, so --name is required the first time",
        ));
    }

    if let Some(d) = description {
        if d.len() > identity::MAX_DESCRIPTION_LEN {
            return Err(io::Error::other(format!(
                "--description must be at most {} bytes (this one is {})",
                identity::MAX_DESCRIPTION_LEN,
                d.len()
            )));
        }
    }

    let now = identity::epoch_ms();
    // An existing hive keeps its id and birthday across a rename. A rename is
    // not a new hive, and peers that know it by id should keep recognising it.
    let (hive_id, created_ms, mut acknowledged) = match &existing {
        Some(e) => (e.hive_id.clone(), e.created_ms, e.open_acknowledged_ms),
        None => (identity::gen_hive_id(), now, None),
    };

    let policy = join_policy
        .or(existing.as_ref().map(|e| e.join_policy))
        .unwrap_or(JoinPolicy::Invite);

    // Consent is required when `open` is being *chosen* here, and also when a
    // previously hand-edited `open` has never been acknowledged — otherwise a
    // `--description` tweak would be a way to leave an unconsented policy in
    // place while the file claims it is set.
    if policy == JoinPolicy::Open && acknowledged.is_none() {
        if !confirm_open(yes)? {
            return Err(io::Error::other(
                "left unchanged — join policy not set to open",
            ));
        }
        acknowledged = Some(now);
    }
    // Moving away from `open` drops the acknowledgement, so coming back to it
    // asks again rather than silently reusing an old consent.
    if policy != JoinPolicy::Open {
        acknowledged = None;
    }

    let next = HiveIdentity {
        hive_id,
        name: name
            .map(str::to_string)
            .or_else(|| existing.as_ref().map(|e| e.name.clone()))
            .unwrap_or_default(),
        description: description
            .map(str::to_string)
            .or_else(|| existing.as_ref().and_then(|e| e.description.clone())),
        join_policy: policy,
        created_ms,
        open_acknowledged_ms: acknowledged,
    };

    identity::save(&next).map_err(io::Error::other)?;

    if json_mode {
        let json = serde_json::json!({
            "hive_id": next.hive_id,
            "name": next.name,
            "description": next.description,
            "join_policy": next.join_policy.as_str(),
            "effective_join_policy": next.effective_join_policy().as_str(),
            "open_acknowledged": next.open_is_acknowledged(),
            "created": existing.is_none(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&json).unwrap_or_default()
        );
        return Ok(());
    }

    if existing.is_none() {
        println!("Named this hive \"{}\" ({}).", next.name, next.hive_id);
    } else {
        println!("Updated \"{}\" ({}).", next.name, next.hive_id);
    }
    println!("Join policy: {}", next.join_policy.as_str());
    println!();
    println!("Advertising it on the LAN is #433, and is not wired up yet — this");
    println!("records the identity that work will read.");
    Ok(())
}

fn clear(yes: bool, json_mode: bool) -> io::Result<()> {
    let Some(existing) = identity::load().map_err(io::Error::other)? else {
        if json_mode {
            println!(
                "{}",
                serde_json::json!({ "named": false, "cleared": false })
            );
        } else {
            println!("This hive is already unnamed — nothing to clear.");
        }
        return Ok(());
    };

    if !yes {
        return Err(io::Error::other(format!(
            "clearing discards the hive id {} — peers who know this hive by it \
             will not recognise a later one. Pass --yes to confirm.",
            existing.hive_id
        )));
    }

    let path = identity::identity_path();
    std::fs::remove_file(&path)
        .map_err(|e| io::Error::other(format!("remove {}: {e}", path.display())))?;

    if json_mode {
        println!(
            "{}",
            serde_json::json!({ "named": false, "cleared": true, "was": existing.hive_id })
        );
    } else {
        println!("Cleared. This hive is unnamed again and advertises nothing.");
    }
    Ok(())
}

/// Print what `open` means and get a yes.
///
/// Returns `Ok(false)` for a refusal, which the caller turns into "left
/// unchanged" — not an error the user has to decode.
fn confirm_open(yes: bool) -> io::Result<bool> {
    if let Some(w) = JoinPolicy::Open.warning() {
        eprintln!();
        eprintln!("WARNING: {w}");
        eprintln!();
        eprintln!("  `ask` gives you the same discoverability and still lets you approve");
        eprintln!("  each request. Prefer it unless you specifically want hands-off joining");
        eprintln!("  on a network you control.");
        eprintln!();
    }

    if yes {
        eprintln!("Proceeding: --yes was passed.");
        return Ok(true);
    }

    // No terminal means no one to answer. Assuming yes in a pipe is how a
    // permissive setting gets made by accident, so require the flag instead.
    if !io::stdin().is_terminal() {
        return Err(io::Error::other(
            "--join-policy open needs confirmation, and there is no terminal to ask on. \
             Pass --yes if you mean it.",
        ));
    }

    crate::init::prompt::yes_no("Open this hive to anyone on your LAN?", false)
}
