use std::convert::Infallible;
use std::mem::swap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use crate::util::trim_lines;

use branchless::commands::undo::testing::{select_past_event, undo_events};
use branchless::core::eventlog::{EventCursor, EventLogDb, EventReplayer};
use branchless::core::formatting::Glyphs;
use branchless::core::mergebase::make_merge_base_db;
use branchless::git::{GitRunInfo, Repo};
use branchless::testing::{make_git, Git};
use branchless::tui::testing::{screen_to_string, CursiveTestingBackend, CursiveTestingEvent};
use branchless::tui::Effects;

use cursive::event::Key;
use cursive::CursiveRunnable;
use os_str_bytes::OsStrBytes;

fn run_select_past_event(
    repo: &Repo,
    events: Vec<CursiveTestingEvent>,
) -> eyre::Result<Option<EventCursor>> {
    let glyphs = Glyphs::text();
    let effects = Effects::new_suppress_for_test(glyphs);
    let conn = repo.get_db_conn()?;
    let event_log_db: EventLogDb = EventLogDb::new(&conn)?;
    let mut event_replayer = EventReplayer::from_event_log_db(&effects, &repo, &event_log_db)?;
    let merge_base_db = make_merge_base_db(&effects, repo, &conn, &event_replayer)?;
    let siv = CursiveRunnable::new::<Infallible, _>(move || {
        Ok(CursiveTestingBackend::init(events.clone()))
    });
    select_past_event(
        siv.into_runner(),
        &effects,
        repo,
        &merge_base_db,
        &mut event_replayer,
    )
}

fn run_undo_events(git: &Git, event_cursor: EventCursor) -> eyre::Result<String> {
    let glyphs = Glyphs::text();
    let effects = Effects::new_suppress_for_test(glyphs.clone());
    let repo = git.get_repo()?;
    let conn = repo.get_db_conn()?;
    let mut event_log_db: EventLogDb = EventLogDb::new(&conn)?;
    let event_replayer = EventReplayer::from_event_log_db(&effects, &repo, &event_log_db)?;
    let input = "y";
    let mut in_ = input.as_bytes();
    let out: Arc<Mutex<Vec<u8>>> = Default::default();

    let git_run_info = GitRunInfo {
        path_to_git: git.path_to_git.clone(),

        // Ensure that nested calls to `git` are run under the correct environment.
        // (Normally, the user will be running `git undo` from the correct directory
        // already.)
        working_directory: repo.get_working_copy_path().unwrap().to_path_buf(),

        // Normally, we want to inherit the user's environment when running external
        // Git commands. However, for testing, we may have inherited variables which
        // affect the execution of Git. In particular, `GIT_INDEX_FILE` is set to
        // `.git/index` normally (which works for the test), but can be set to an
        // absolute path when running `git commit -a`, and having these tests run as
        // part of a commit hook.
        env: std::env::vars_os()
            .filter(|(k, _v)| !k.to_raw_bytes().starts_with(b"GIT_"))
            .collect(),
    };

    let result = undo_events(
        &mut in_,
        &Effects::new_from_buffer_for_test(glyphs, &out),
        &repo,
        &git_run_info,
        &mut event_log_db,
        &event_replayer,
        event_cursor,
    )?;
    assert_eq!(result, 0);

    let out = {
        let mut buf = out.lock().unwrap();
        let mut result_buf = Vec::new();
        swap(&mut *buf, &mut result_buf);
        result_buf
    };
    let out = String::from_utf8(out)?;
    let out = git.preprocess_output(out)?;
    let out = trim_lines(out);
    Ok(out)
}

#[test]
fn test_undo_help() -> eyre::Result<()> {
    let git = make_git()?;

    git.init_repo()?;

    {
        let screenshot1 = Default::default();
        run_select_past_event(
            &git.get_repo()?,
            vec![
                CursiveTestingEvent::Event('h'.into()),
                CursiveTestingEvent::TakeScreenshot(Rc::clone(&screenshot1)),
                CursiveTestingEvent::Event('q'.into()),
            ],
        )?;
        insta::assert_snapshot!(screen_to_string(&screenshot1), @r###"
        ┌───────────────────────────────────────────────────┤─Commit graph ├───────────────────────────────────────────────────┐
        │O f777ecc9 (master) create initial.txt                                                                                │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │        ┌───────────────────────────────────────────┤─How to use ├───────────────────────────────────────────┐        │
        │        │ Use `git undo` to view and revert to previous states of the repository.                            │        │
        │        │                                                                                                    │        │
        │        │ h/?: Show this help.                                                                               │        │
        │        │ q: Quit.                                                                                           │        │
        │        │ p/n or <left>/<right>: View next/previous state.                                                   │        │
        │        │ g: Go to a provided event ID.                                                                      │        │
        │        │ <enter>: Revert the repository to the given state (requires confirmation).                         │        │
        │        │                                                                                                    │        │
        │        │ You can also copy a commit hash from the past and manually run `git unhide` or `git rebase` on it. │        │
        │        │                                                                                                    │        │
        │        │                                                                                            <Close> │        │
        │        └────────────────────────────────────────────────────────────────────────────────────────────────────┘        │
        │                                                                                                                      │
        │                                                                                                                      │
        └──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌──────────────────────────────────────────────────────┤─Events ├──────────────────────────────────────────────────────┐
        │There are no previous available events.                                                                               │
        └──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
        "###);
    }

    Ok(())
}

#[test]
fn test_undo_navigate() -> eyre::Result<()> {
    let git = make_git()?;

    if !git.supports_reference_transactions()? {
        return Ok(());
    }

    git.init_repo()?;
    git.commit_file("test1", 1)?;
    git.commit_file("test2", 2)?;

    {
        let screenshot1 = Default::default();
        let screenshot2 = Default::default();
        let event_cursor = run_select_past_event(
            &git.get_repo()?,
            vec![
                CursiveTestingEvent::Event('p'.into()),
                CursiveTestingEvent::TakeScreenshot(Rc::clone(&screenshot1)),
                CursiveTestingEvent::Event('n'.into()),
                CursiveTestingEvent::TakeScreenshot(Rc::clone(&screenshot2)),
                CursiveTestingEvent::Event(Key::Enter.into()),
            ],
        )?;
        insta::assert_debug_snapshot!(event_cursor, @r###"
            Some(
                EventCursor {
                    event_id: 6,
                },
            )
            "###);
        insta::assert_snapshot!(screen_to_string(&screenshot1), @r###"
        ┌───────────────────────────────────────────────────┤─Commit graph ├───────────────────────────────────────────────────┐
        │:                                                                                                                     │
        │@ 96d1c37a (master) create test2.txt                                                                                  │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        └──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌──────────────────────────────────────────────────────┤─Events ├──────────────────────────────────────────────────────┐
        │Repo after transaction 3 (event 4). Press 'h' for help, 'q' to quit.                                                  │
        │1. Check out from 62fc20d2 create test1.txt                                                                           │
        │               to 96d1c37a create test2.txt                                                                           │
        │2. Move branch master from 62fc20d2 create test1.txt                                                                  │
        │                        to 96d1c37a create test2.txt                                                                  │
        └──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
        "###);
        insta::assert_snapshot!(screen_to_string(&screenshot2), @r###"
        ┌───────────────────────────────────────────────────┤─Commit graph ├───────────────────────────────────────────────────┐
        │:                                                                                                                     │
        │@ 96d1c37a (master) create test2.txt                                                                                  │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        └──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌──────────────────────────────────────────────────────┤─Events ├──────────────────────────────────────────────────────┐
        │Repo after transaction 4 (event 6). Press 'h' for help, 'q' to quit.                                                  │
        │1. Commit 96d1c37a create test2.txt                                                                                   │
        │                                                                                                                      │
        └──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
        "###);
    };

    Ok(())
}

#[test]
fn test_go_to_event() -> eyre::Result<()> {
    let git = make_git()?;

    if !git.supports_reference_transactions()? {
        return Ok(());
    }

    git.init_repo()?;
    git.commit_file("test1", 1)?;
    git.commit_file("test2", 2)?;

    let screenshot1 = Default::default();
    let screenshot2 = Default::default();
    run_select_past_event(
        &git.get_repo()?,
        vec![
            CursiveTestingEvent::TakeScreenshot(Rc::clone(&screenshot1)),
            CursiveTestingEvent::Event('g'.into()),
            CursiveTestingEvent::Event('1'.into()),
            CursiveTestingEvent::Event(Key::Enter.into()),
            CursiveTestingEvent::TakeScreenshot(Rc::clone(&screenshot2)),
            CursiveTestingEvent::Event('q'.into()),
        ],
    )?;

    insta::assert_snapshot!(screen_to_string(&screenshot1), @r###"
    ┌───────────────────────────────────────────────────┤─Commit graph ├───────────────────────────────────────────────────┐
    │:                                                                                                                     │
    │@ 96d1c37a (master) create test2.txt                                                                                  │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    └──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
    ┌──────────────────────────────────────────────────────┤─Events ├──────────────────────────────────────────────────────┐
    │Repo after transaction 4 (event 6). Press 'h' for help, 'q' to quit.                                                  │
    │1. Commit 96d1c37a create test2.txt                                                                                   │
    │                                                                                                                      │
    └──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
    "###);
    insta::assert_snapshot!(screen_to_string(&screenshot2), @r###"
    ┌───────────────────────────────────────────────────┤─Commit graph ├───────────────────────────────────────────────────┐
    │:                                                                                                                     │
    │@ 62fc20d2 create test1.txt                                                                                           │
    │|                                                                                                                     │
    │O 96d1c37a (master) create test2.txt                                                                                  │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    └──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
    ┌──────────────────────────────────────────────────────┤─Events ├──────────────────────────────────────────────────────┐
    │Repo after transaction 1 (event 1). Press 'h' for help, 'q' to quit.                                                  │
    │1. Check out from f777ecc9 create initial.txt                                                                         │
    │               to 62fc20d2 create test1.txt                                                                           │
    └──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
    "###);

    Ok(())
}

#[test]
fn test_undo_hide() -> eyre::Result<()> {
    let git = make_git()?;

    if !git.supports_reference_transactions()? {
        return Ok(());
    }

    git.init_repo()?;
    git.run(&["checkout", "-b", "test1"])?;
    git.commit_file("test1", 1)?;
    git.run(&["checkout", "HEAD^"])?;
    git.commit_file("test2", 2)?;
    git.run(&["hide", "test1"])?;
    git.run(&["branch", "-D", "test1"])?;

    {
        let (stdout, stderr) = git.run(&["smartlog"])?;
        insta::assert_snapshot!(stderr, @"");
        insta::assert_snapshot!(stdout, @r###"
            O f777ecc9 (master) create initial.txt
            |
            @ fe65c1fe create test2.txt
            "###);
    }

    let event_cursor = run_select_past_event(
        &git.get_repo()?,
        vec![
            CursiveTestingEvent::Event('p'.into()),
            CursiveTestingEvent::Event('p'.into()),
            CursiveTestingEvent::Event(Key::Enter.into()),
            CursiveTestingEvent::Event('y'.into()),
        ],
    )?;
    insta::assert_debug_snapshot!(event_cursor, @r###"
        Some(
            EventCursor {
                event_id: 9,
            },
        )
        "###);
    let event_cursor = event_cursor.unwrap();

    {
        let stdout = run_undo_events(&git, event_cursor)?;
        insta::assert_snapshot!(stdout, @r###"
            Will apply these actions:
            1. Create branch test1 at 62fc20d2 create test1.txt

            2. Unhide commit 62fc20d2 create test1.txt

            Confirm? [yN] Applied 2 inverse events.
            "###);
    }

    {
        let (stdout, _stderr) = git.run(&["smartlog"])?;
        insta::assert_snapshot!(stdout, @r###"
            O f777ecc9 (master) create initial.txt
            |\
            | o 62fc20d2 (test1) create test1.txt
            |
            @ fe65c1fe create test2.txt
            "###);
    }

    Ok(())
}

#[test]
fn test_undo_move_refs() -> eyre::Result<()> {
    let git = make_git()?;

    if !git.supports_reference_transactions()? {
        return Ok(());
    }

    git.init_repo()?;
    git.commit_file("test1", 1)?;
    git.commit_file("test2", 2)?;

    let event_cursor = run_select_past_event(
        &git.get_repo()?,
        vec![
            CursiveTestingEvent::Event('p'.into()),
            CursiveTestingEvent::Event('p'.into()),
            CursiveTestingEvent::Event(Key::Enter.into()),
            CursiveTestingEvent::Event('y'.into()),
        ],
    )?;
    insta::assert_debug_snapshot!(event_cursor, @r###"
        Some(
            EventCursor {
                event_id: 3,
            },
        )
        "###);
    let event_cursor = event_cursor.unwrap();

    {
        let stdout = run_undo_events(&git, event_cursor)?;
        insta::assert_snapshot!(stdout, @r###"
        Will apply these actions:
        1. Check out from 96d1c37a create test2.txt
                       to 62fc20d2 create test1.txt
        2. Hide commit 96d1c37a create test2.txt

        3. Move branch master from 96d1c37a create test2.txt
                                to 62fc20d2 create test1.txt
        Confirm? [yN] branchless: running command: <git-executable> checkout --detach 62fc20d2a290daea0d52bdc2ed2ad4be6491010e
        Applied 3 inverse events.
        "###);
    }

    {
        let (stdout, _stderr) = git.run(&["smartlog"])?;
        insta::assert_snapshot!(stdout, @r###"
            :
            @ 62fc20d2 (master) create test1.txt
            "###);
    }

    Ok(())
}

#[test]
fn test_historical_smartlog_visibility() -> eyre::Result<()> {
    let git = make_git()?;

    git.init_repo()?;
    git.commit_file("test1", 1)?;
    git.run(&["hide", "HEAD"])?;

    let screenshot1 = Default::default();
    let screenshot2 = Default::default();
    run_select_past_event(
        &git.get_repo()?,
        vec![
            CursiveTestingEvent::TakeScreenshot(Rc::clone(&screenshot1)),
            CursiveTestingEvent::Event('p'.into()),
            CursiveTestingEvent::TakeScreenshot(Rc::clone(&screenshot2)),
            CursiveTestingEvent::Event('q'.into()),
        ],
    )?;

    if git.supports_reference_transactions()? {
        insta::assert_snapshot!(screen_to_string(&screenshot1), @r###"
        ┌───────────────────────────────────────────────────┤─Commit graph ├───────────────────────────────────────────────────┐
        │:                                                                                                                     │
        │% 62fc20d2 (manually hidden) (master) create test1.txt                                                                │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        └──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌──────────────────────────────────────────────────────┤─Events ├──────────────────────────────────────────────────────┐
        │Repo after transaction 3 (event 4). Press 'h' for help, 'q' to quit.                                                  │
        │1. Hide commit 62fc20d2 create test1.txt                                                                              │
        │                                                                                                                      │
        └──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
        "###);
        insta::assert_snapshot!(screen_to_string(&screenshot2), @r###"
        ┌───────────────────────────────────────────────────┤─Commit graph ├───────────────────────────────────────────────────┐
        │:                                                                                                                     │
        │@ 62fc20d2 (master) create test1.txt                                                                                  │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        └──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌──────────────────────────────────────────────────────┤─Events ├──────────────────────────────────────────────────────┐
        │Repo after transaction 2 (event 3). Press 'h' for help, 'q' to quit.                                                  │
        │1. Commit 62fc20d2 create test1.txt                                                                                   │
        │                                                                                                                      │
        └──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
        "###);
    } else {
        insta::assert_snapshot!(screen_to_string(&screenshot1), @r###"
        ┌───────────────────────────────────────────────────┤─Commit graph ├───────────────────────────────────────────────────┐
        │:                                                                                                                     │
        │% 62fc20d2 (manually hidden) (master) create test1.txt                                                                │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        └──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌──────────────────────────────────────────────────────┤─Events ├──────────────────────────────────────────────────────┐
        │Repo after transaction 2 (event 2). Press 'h' for help, 'q' to quit.                                                  │
        │1. Hide commit 62fc20d2 create test1.txt                                                                              │
        │                                                                                                                      │
        └──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
        "###);
        insta::assert_snapshot!(screen_to_string(&screenshot2), @r###"
        ┌───────────────────────────────────────────────────┤─Commit graph ├───────────────────────────────────────────────────┐
        │:                                                                                                                     │
        │@ 62fc20d2 (master) create test1.txt                                                                                  │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        │                                                                                                                      │
        └──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌──────────────────────────────────────────────────────┤─Events ├──────────────────────────────────────────────────────┐
        │Repo after transaction 1 (event 1). Press 'h' for help, 'q' to quit.                                                  │
        │1. Commit 62fc20d2 create test1.txt                                                                                   │
        │                                                                                                                      │
        └──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
        "###);
    }

    Ok(())
}

#[test]
fn test_undo_doesnt_make_working_dir_dirty() -> eyre::Result<()> {
    let git = make_git()?;

    if !git.supports_reference_transactions()? {
        return Ok(());
    }

    git.init_repo()?;

    // Modify a reference.
    git.run(&["branch", "foo"])?;
    // Make a change that causes a checkout.
    git.commit_file("test1", 1)?;
    // Modify a reference.
    git.run(&["branch", "bar"])?;

    let screenshot1 = Default::default();
    let event_cursor = run_select_past_event(
        &git.get_repo()?,
        vec![
            CursiveTestingEvent::Event('p'.into()),
            CursiveTestingEvent::Event('p'.into()),
            CursiveTestingEvent::Event('p'.into()),
            CursiveTestingEvent::Event('p'.into()),
            CursiveTestingEvent::TakeScreenshot(Rc::clone(&screenshot1)),
            CursiveTestingEvent::Event(Key::Enter.into()),
        ],
    )?;
    insta::assert_snapshot!(screen_to_string(&screenshot1), @r###"
    ┌───────────────────────────────────────────────────┤─Commit graph ├───────────────────────────────────────────────────┐
    │:                                                                                                                     │
    │O 62fc20d2 (master) create test1.txt                                                                                  │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    └──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
    ┌──────────────────────────────────────────────────────┤─Events ├──────────────────────────────────────────────────────┐
    │There are no previous available events.                                                                               │
    └──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
    "###);

    // If there are no dirty files in the repository prior to the `undo`,
    // then there should still be no dirty files after the `undo`.
    let event_cursor = event_cursor.expect("Should have an event cursor to undo");
    {
        let (stdout, _stderr) = git.run(&["status", "--porcelain"])?;
        assert_eq!(stdout, "");
    }
    {
        let stdout = run_undo_events(&git, event_cursor)?;
        insta::assert_snapshot!(stdout, @r###"
        Will apply these actions:
        1. Check out from 62fc20d2 create test1.txt
                       to f777ecc9 create initial.txt
        2. Delete branch bar at 62fc20d2 create test1.txt

        3. Hide commit 62fc20d2 create test1.txt

        4. Move branch master from 62fc20d2 create test1.txt
                                to f777ecc9 create initial.txt
        5. Delete branch foo at f777ecc9 create initial.txt

        Confirm? [yN] branchless: running command: <git-executable> checkout --detach f777ecc9b0db5ed372b2615695191a8a17f79f24
        Applied 5 inverse events.
        "###);
    }
    {
        let (stdout, _stderr) = git.run(&["status", "--porcelain"])?;
        assert_eq!(stdout, "");
    }

    Ok(())
}

/// See https://github.com/arxanas/git-branchless/issues/57
#[cfg(unix)]
#[test]
fn test_git_bisect_produces_empty_event() -> eyre::Result<()> {
    let git = make_git()?;

    if !git.supports_reference_transactions()? {
        return Ok(());
    }
    git.init_repo()?;

    git.commit_file("test1", 1)?;
    git.run(&["bisect", "start"])?;
    git.run(&["bisect", "good", "HEAD^"])?;
    git.run(&["bisect", "bad"])?;

    let screenshot1 = Default::default();
    run_select_past_event(
        &git.get_repo()?,
        vec![
            CursiveTestingEvent::Event('p'.into()),
            CursiveTestingEvent::Event('p'.into()),
            CursiveTestingEvent::TakeScreenshot(Rc::clone(&screenshot1)),
            CursiveTestingEvent::Event(Key::Enter.into()),
        ],
    )?;
    insta::assert_snapshot!(screen_to_string(&screenshot1), @r###"
    ┌───────────────────────────────────────────────────┤─Commit graph ├───────────────────────────────────────────────────┐
    │:                                                                                                                     │
    │@ 62fc20d2 (master) create test1.txt                                                                                  │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    │                                                                                                                      │
    └──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
    ┌──────────────────────────────────────────────────────┤─Events ├──────────────────────────────────────────────────────┐
    │Repo after transaction 3 (event 4). Press 'h' for help, 'q' to quit.                                                  │
    │1. Empty event for BISECT_HEAD                                                                                        │
    │   This may be an unsupported use-case; see https://git.io/J0b7z                                                      │
    └──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
    "###);

    Ok(())
}
