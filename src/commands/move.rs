//! Move commits and subtrees from one place to another.
//!
//! Under the hood, this makes use of Git's advanced rebase functionality, which
//! is also used to preserve merge commits using the `--rebase-merges` option.

use std::fmt::Write;
use std::time::SystemTime;

use tracing::instrument;

use crate::core::config::get_restack_preserve_timestamps;
use crate::core::eventlog::{EventLogDb, EventReplayer};
use crate::core::graph::{
    make_graph, resolve_commits, BranchOids, CommitGraph, HeadOid, MainBranchOid,
    ResolveCommitsResult,
};
use crate::core::mergebase::{make_merge_base_db, MergeBaseDb};
use crate::core::rewrite::{
    execute_rebase_plan, BuildRebasePlanOptions, ExecuteRebasePlanOptions, RebasePlanBuilder,
};
use crate::git::{GitRunInfo, NonZeroOid, Repo};
use crate::tui::Effects;

#[instrument]
fn resolve_base_commit(
    graph: &CommitGraph,
    merge_base_oid: Option<NonZeroOid>,
    oid: NonZeroOid,
) -> NonZeroOid {
    let node = &graph[&oid];
    if node.is_main {
        oid
    } else {
        match node.parent {
            Some(parent_oid) => {
                if graph[&parent_oid].is_main || Some(parent_oid) == merge_base_oid {
                    oid
                } else {
                    resolve_base_commit(graph, merge_base_oid, parent_oid)
                }
            }
            None => oid,
        }
    }
}

/// Move a subtree from one place to another.
#[instrument]
pub fn r#move(
    effects: &Effects,
    git_run_info: &GitRunInfo,
    source: Option<String>,
    dest: Option<String>,
    base: Option<String>,
    force_in_memory: bool,
    force_on_disk: bool,
    dump_rebase_constraints: bool,
    dump_rebase_plan: bool,
) -> eyre::Result<isize> {
    let repo = Repo::from_current_dir()?;
    let head_oid = repo.get_head_info()?.oid;
    let (source, should_resolve_base_commit) = match (source, base) {
        (Some(_), Some(_)) => {
            writeln!(
                effects.get_output_stream(),
                "The --source and --base options cannot both be provided."
            )?;
            return Ok(1);
        }
        (Some(source), None) => (source, false),
        (None, Some(base)) => (base, true),
        (None, None) => {
            let source_oid = match head_oid {
                Some(oid) => oid,
                None => {
                    writeln!(effects.get_output_stream(), "No --source or --base argument was provided, and no OID for HEAD is available as a default")?;
                    return Ok(1);
                }
            };
            (source_oid.to_string(), true)
        }
    };
    let dest = match dest {
        Some(dest) => dest,
        None => match head_oid {
            Some(oid) => oid.to_string(),
            None => {
                writeln!(effects.get_output_stream(), "No --dest argument was provided, and no OID for HEAD is available as a default")?;
                return Ok(1);
            }
        },
    };
    let (source_oid, dest_oid) = match resolve_commits(&repo, vec![source, dest])? {
        ResolveCommitsResult::Ok { commits } => match &commits.as_slice() {
            [source_commit, dest_commit] => (source_commit.get_oid(), dest_commit.get_oid()),
            _ => eyre::bail!("Unexpected number of returns values from resolve_commits"),
        },
        ResolveCommitsResult::CommitNotFound { commit } => {
            writeln!(effects.get_output_stream(), "Commit not found: {}", commit)?;
            return Ok(1);
        }
    };

    let main_branch_oid = repo.get_main_branch_oid()?;
    let branch_oid_to_names = repo.get_branch_oid_to_names()?;
    let conn = repo.get_db_conn()?;
    let event_log_db = EventLogDb::new(&conn)?;
    let event_replayer = EventReplayer::from_event_log_db(effects, &repo, &event_log_db)?;
    let event_cursor = event_replayer.make_default_cursor();
    let merge_base_db = make_merge_base_db(effects, &repo, &conn, &event_replayer)?;
    let graph = make_graph(
        effects,
        &repo,
        &merge_base_db,
        &event_replayer,
        event_cursor,
        &HeadOid(Some(source_oid)),
        &MainBranchOid(main_branch_oid),
        &BranchOids(branch_oid_to_names.keys().copied().collect()),
        true,
    )?;

    let source_oid = if should_resolve_base_commit {
        let merge_base_oid =
            merge_base_db.get_merge_base_oid(effects, &repo, source_oid, dest_oid)?;
        resolve_base_commit(&graph, merge_base_oid, source_oid)
    } else {
        source_oid
    };

    let now = SystemTime::now();
    let event_tx_id = event_log_db.make_transaction_id(now, "move")?;
    let rebase_plan = {
        let mut builder = RebasePlanBuilder::new(
            &repo,
            &graph,
            &merge_base_db,
            &MainBranchOid(main_branch_oid),
        );
        builder.move_subtree(source_oid, dest_oid)?;
        builder.build(
            effects,
            &BuildRebasePlanOptions {
                dump_rebase_constraints,
                dump_rebase_plan,
                detect_duplicate_commits_via_patch_id: true,
            },
        )?
    };
    let result = match rebase_plan {
        Ok(None) => {
            writeln!(effects.get_output_stream(), "Nothing to do.")?;
            0
        }
        Ok(Some(rebase_plan)) => {
            let options = ExecuteRebasePlanOptions {
                now,
                event_tx_id,
                preserve_timestamps: get_restack_preserve_timestamps(&repo)?,
                force_in_memory,
                force_on_disk,
            };
            execute_rebase_plan(effects, git_run_info, &repo, &rebase_plan, &options)?
        }
        Err(err) => {
            err.describe(effects, &repo)?;
            1
        }
    };
    Ok(result)
}
