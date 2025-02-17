//! Handle hiding commits when explicitly requested by the user (as opposed to
//! automatically as the result of a rewrite operation).

use std::collections::HashSet;
use std::fmt::Write;
use std::time::SystemTime;

use tracing::instrument;

use crate::core::eventlog::{CommitVisibility, Event};
use crate::core::eventlog::{EventLogDb, EventReplayer};
use crate::core::formatting::{printable_styled_string, Glyphs};
use crate::core::graph::{
    make_graph, resolve_commits, BranchOids, CommitGraph, HeadOid, MainBranchOid, Node,
    ResolveCommitsResult,
};
use crate::core::mergebase::{make_merge_base_db, MergeBaseDb};
use crate::core::metadata::{render_commit_metadata, CommitOidProvider};
use crate::git::{Commit, Repo};
use crate::tui::Effects;

fn recurse_on_commits_helper<
    'repo,
    'graph,
    Condition: Fn(&'graph Node<'repo>) -> bool,
    Callback: FnMut(&'graph Node<'repo>),
>(
    graph: &'graph CommitGraph<'repo>,
    condition: &Condition,
    commit: &Commit<'repo>,
    callback: &mut Callback,
) {
    let node = &graph[&commit.get_oid()];
    if condition(node) {
        callback(node);
    };

    for child_oid in node.children.iter() {
        let child_commit = &graph[child_oid].commit;
        recurse_on_commits_helper(graph, condition, child_commit, callback)
    }
}

fn recurse_on_commits<'repo, F: Fn(&Node) -> bool>(
    effects: &Effects,
    repo: &'repo Repo,
    merge_base_db: &impl MergeBaseDb,
    event_replayer: &EventReplayer,
    commits: Vec<Commit<'repo>>,
    condition: F,
) -> eyre::Result<Vec<Commit<'repo>>> {
    let head_oid = repo.get_head_info()?.oid;
    let main_branch_oid = repo.get_main_branch_oid()?;
    let branch_oid_to_names = repo.get_branch_oid_to_names()?;
    let graph = make_graph(
        effects,
        repo,
        merge_base_db,
        event_replayer,
        event_replayer.make_default_cursor(),
        &HeadOid(head_oid),
        &MainBranchOid(main_branch_oid),
        &BranchOids(branch_oid_to_names.keys().copied().collect()),
        false,
    )?;

    // Maintain ordering, since it's likely to be meaningful.
    let mut result: Vec<Commit<'repo>> = Vec::new();
    let mut seen_oids = HashSet::new();
    for commit in commits {
        recurse_on_commits_helper(&graph, &condition, &commit, &mut |child_node| {
            let child_commit = &child_node.commit;
            if !seen_oids.contains(&child_commit.get_oid()) {
                seen_oids.insert(child_commit.get_oid());
                result.push(child_commit.clone());
            }
        });
    }
    Ok(result)
}

/// Hide the hashes provided on the command-line.
#[instrument]
pub fn hide(effects: &Effects, hashes: Vec<String>, recursive: bool) -> eyre::Result<isize> {
    let now = SystemTime::now();
    let glyphs = Glyphs::detect();
    let repo = Repo::from_current_dir()?;
    let conn = repo.get_db_conn()?;
    let mut event_log_db = EventLogDb::new(&conn)?;
    let event_replayer = EventReplayer::from_event_log_db(effects, &repo, &event_log_db)?;
    let merge_base_db = make_merge_base_db(effects, &repo, &conn, &event_replayer)?;

    let commits = resolve_commits(&repo, hashes)?;
    let commits = match commits {
        ResolveCommitsResult::Ok { commits } => commits,
        ResolveCommitsResult::CommitNotFound { commit: hash } => {
            writeln!(effects.get_output_stream(), "Commit not found: {}", hash)?;
            return Ok(1);
        }
    };
    let commits = if recursive {
        recurse_on_commits(
            effects,
            &repo,
            &merge_base_db,
            &event_replayer,
            commits,
            |node| node.is_visible,
        )?
    } else {
        commits
    };

    let timestamp = now.duration_since(SystemTime::UNIX_EPOCH)?.as_secs_f64();
    let event_tx_id = event_log_db.make_transaction_id(now, "hide")?;
    let events = commits
        .iter()
        .map(|commit| Event::HideEvent {
            timestamp,
            event_tx_id,
            commit_oid: commit.get_oid(),
        })
        .collect();
    event_log_db.add_events(events)?;

    let cursor = event_replayer.make_default_cursor();
    for commit in commits {
        writeln!(
            effects.get_output_stream(),
            "Hid commit: {}",
            printable_styled_string(&glyphs, commit.friendly_describe()?)?
        )?;
        if let Some(CommitVisibility::Hidden) =
            event_replayer.get_cursor_commit_visibility(cursor, commit.get_oid())
        {
            writeln!(
                effects.get_output_stream(),
                "(It was already hidden, so this operation had no effect.)"
            )?;
        }

        let commit_target_oid =
            render_commit_metadata(&commit, &mut [&mut CommitOidProvider::new(false)?])?;
        writeln!(
            effects.get_output_stream(),
            "To unhide this commit, run: git unhide {}",
            printable_styled_string(&glyphs, commit_target_oid)?
        )?;
    }

    Ok(0)
}

/// Unhide the hashes provided on the command-line.
#[instrument]
pub fn unhide(effects: &Effects, hashes: Vec<String>, recursive: bool) -> eyre::Result<isize> {
    let now = SystemTime::now();
    let glyphs = Glyphs::detect();
    let repo = Repo::from_current_dir()?;
    let conn = repo.get_db_conn()?;
    let mut event_log_db = EventLogDb::new(&conn)?;
    let event_replayer = EventReplayer::from_event_log_db(effects, &repo, &event_log_db)?;
    let merge_base_db = make_merge_base_db(effects, &repo, &conn, &event_replayer)?;

    let commits = resolve_commits(&repo, hashes)?;
    let commits = match commits {
        ResolveCommitsResult::Ok { commits } => commits,
        ResolveCommitsResult::CommitNotFound { commit: hash } => {
            writeln!(effects.get_output_stream(), "Commit not found: {}", hash)?;
            return Ok(1);
        }
    };
    let commits = if recursive {
        recurse_on_commits(
            effects,
            &repo,
            &merge_base_db,
            &event_replayer,
            commits,
            |node| !node.is_visible,
        )?
    } else {
        commits
    };

    let timestamp = now.duration_since(SystemTime::UNIX_EPOCH)?.as_secs_f64();
    let event_tx_id = event_log_db.make_transaction_id(now, "unhide")?;
    let events = commits
        .iter()
        .map(|commit| Event::UnhideEvent {
            timestamp,
            event_tx_id,
            commit_oid: commit.get_oid(),
        })
        .collect();
    event_log_db.add_events(events)?;

    let cursor = event_replayer.make_default_cursor();
    for commit in commits {
        writeln!(
            effects.get_output_stream(),
            "Unhid commit: {}",
            printable_styled_string(&glyphs, commit.friendly_describe()?)?,
        )?;
        if let Some(CommitVisibility::Visible) =
            event_replayer.get_cursor_commit_visibility(cursor, commit.get_oid())
        {
            writeln!(
                effects.get_output_stream(),
                "(It was not hidden, so this operation had no effect.)"
            )?;
        }

        let commit_target_oid =
            render_commit_metadata(&commit, &mut [&mut CommitOidProvider::new(false)?])?;
        writeln!(
            effects.get_output_stream(),
            "To hide this commit, run: git hide {}",
            printable_styled_string(&glyphs, commit_target_oid)?
        )?;
    }

    Ok(0)
}
