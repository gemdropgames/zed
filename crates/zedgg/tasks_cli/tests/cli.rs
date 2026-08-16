//! Integration tests running the built `zedgg-tasks` binary against a real
//! `zedgg.sqlite` in a temp directory. No TTY, so `delete` must be forced.

// Blocking process spawns are disallowed in app code (they stall the UI
// thread); this is a synchronous test binary, same carve-out as
// `zed_visual_test_runner`.
#![allow(clippy::disallowed_methods)]

use std::path::Path;
use std::process::{Command, Output};

use zedgg_project_db::tasks;

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zedgg-tasks"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("binary runs")
}

fn stdout_json(output: &Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "expected success, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("stdout is JSON")
}

#[test]
fn full_round_trip_with_json_output() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_str().unwrap();

    // create (with --root against an empty dir) creates the DB.
    let created = run(
        dir.path(),
        &[
            "--root", root, "--json", "create", "Ship the demo",
            "-d", "# Steps\n- record\n",
            "--tag", "video",
        ],
    );
    let created = stdout_json(&created);
    let id = created["id"].as_i64().expect("create returns id");
    let id_arg = id.to_string();

    // list shows it in backlog with its tag.
    let list = stdout_json(&run(dir.path(), &["--json", "list"]));
    let row = &list.as_array().expect("list is array")[0];
    assert_eq!(row["id"].as_i64(), Some(id));
    assert_eq!(row["state"], "backlog");
    assert_eq!(row["title"], "Ship the demo");
    assert_eq!(row["tags"][0], "video");

    // move to review, then filter by state.
    let moved = stdout_json(&run(dir.path(), &["--json", "move", &id_arg, "review"]));
    assert_eq!(moved["state"], "review");
    let review = stdout_json(&run(dir.path(), &["--json", "list", "--state", "review"]));
    assert_eq!(review.as_array().unwrap().len(), 1);
    let backlog = stdout_json(&run(dir.path(), &["--json", "list", "--state", "backlog"]));
    assert_eq!(backlog.as_array().unwrap().len(), 0);

    // tag/untag by name; unknown tag names are created.
    stdout_json(&run(dir.path(), &["--json", "tag", &id_arg, "audio"]));
    let shown = stdout_json(&run(dir.path(), &["--json", "show", &id_arg]));
    assert_eq!(shown["description"], "# Steps\n- record\n");
    assert_eq!(shown["tags"], serde_json::json!(["audio", "video"]));
    stdout_json(&run(dir.path(), &["--json", "untag", &id_arg, "video"]));
    let shown = stdout_json(&run(dir.path(), &["--json", "show", &id_arg]));
    assert_eq!(shown["tags"], serde_json::json!(["audio"]));

    // delete without --force refuses on a non-TTY and changes nothing.
    let refused = run(dir.path(), &["delete", &id_arg]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("--force"),
        "stderr names the fix"
    );
    let connection = zedgg_project_db::open(dir.path()).unwrap();
    assert!(tasks::get_task(&connection, id).unwrap().is_some());

    // delete --force removes it.
    stdout_json(&run(dir.path(), &["--json", "delete", &id_arg, "--force"]));
    assert!(tasks::get_task(&connection, id).unwrap().is_none());
}

#[test]
fn walks_up_from_a_subdirectory() {
    let dir = tempfile::tempdir().unwrap();
    let connection = zedgg_project_db::open(dir.path()).unwrap();
    tasks::create_task(&connection, "from below").unwrap();
    let nested = dir.path().join("assets/sprites");
    std::fs::create_dir_all(&nested).unwrap();

    let list = stdout_json(&run(&nested, &["--json", "list"]));
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["title"], "from below");
}

#[test]
fn reads_never_create_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let output = run(dir.path(), &["list"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("zedgg.sqlite"),
        "error names the missing file"
    );
    assert!(!dir.path().join("zedgg.sqlite").exists());

    // Same with --root: listing must not create.
    let root = dir.path().to_str().unwrap().to_string();
    let output = run(dir.path(), &["--root", &root, "list"]);
    assert!(!output.status.success());
    assert!(!dir.path().join("zedgg.sqlite").exists());
}

#[test]
fn json_errors_go_to_stderr_as_json() {
    let dir = tempfile::tempdir().unwrap();
    let output = run(dir.path(), &["--json", "list"]);
    assert!(!output.status.success());
    let error: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("stderr is JSON in --json mode");
    assert!(error["error"].as_str().unwrap().contains("zedgg.sqlite"));
    assert!(output.stdout.is_empty());
}

#[test]
fn human_output_lists_states_and_titles() {
    let dir = tempfile::tempdir().unwrap();
    let connection = zedgg_project_db::open(dir.path()).unwrap();
    let id = tasks::create_task(&connection, "readable").unwrap();
    tasks::move_task_between(&connection, id, tasks::TaskState::InProgress, None, None).unwrap();

    let output = run(dir.path(), &["list"]);
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(text.contains("In Progress"));
    assert!(text.contains("readable"));
    assert!(text.contains(&format!("#{id}")));
}
