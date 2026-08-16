//! `zedgg-tasks`: manage the tasks in a project's `zedgg.sqlite` from the
//! shell. Thin wrapper over `zedgg_project_db::tasks`; the GUI panel picks
//! up CLI writes through its DB-change watcher. See `AGENTS.md` for the
//! agent-facing contract (`--json`, `--force`, exit codes).

use std::path::PathBuf;

use anyhow::{Context as _, Result, bail};
use clap::{Parser, Subcommand};
use serde_json::json;
use zedgg_project_db::tasks::{self, TAG_COLORS, Tag, TaskState};
use zedgg_project_db::{Connection, DB_FILE, open, open_existing};

#[derive(Parser)]
#[command(name = "zedgg-tasks", about = "Manage ZedGG tasks in a project's zedgg.sqlite")]
struct Cli {
    /// Project root containing zedgg.sqlite (default: walk up from the
    /// current directory). Only an explicit --root may create a fresh
    /// database, and only for commands that write.
    #[arg(long, global = true)]
    root: Option<PathBuf>,

    /// Emit machine-readable JSON on stdout (errors become JSON on stderr)
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List tasks, grouped by state
    List {
        /// Only this state (backlog|in_progress|review|done)
        #[arg(long)]
        state: Option<String>,
    },
    /// Print one task in full: fields, tags, description, attachments
    Show { id: i64 },
    /// Create a task at the top of Backlog; prints its id
    Create {
        title: String,
        /// Markdown description body
        #[arg(short = 'd', long)]
        description: Option<String>,
        /// Start in this state instead of backlog
        #[arg(long)]
        state: Option<String>,
        /// Assign a tag (repeatable); unknown names are created
        #[arg(long = "tag")]
        tags: Vec<String>,
    },
    /// Delete a task (cascades tags assignments and attachments)
    Delete {
        id: i64,
        /// Skip the confirmation prompt (required when stdin is not a TTY)
        #[arg(long)]
        force: bool,
    },
    /// Move a task to the end of a state's column
    Move { id: i64, state: String },
    /// Assign a tag by name; unknown names are created
    Tag { id: i64, name: String },
    /// Unassign a tag by name
    Untag { id: i64, name: String },
}

fn main() {
    let cli = Cli::parse();
    let json = cli.json;
    if let Err(error) = run(cli) {
        if json {
            eprintln!("{}", json!({ "error": format!("{error:#}") }));
        } else {
            eprintln!("error: {error:#}");
        }
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let root = resolve_root(cli.root.as_deref().map(PathBuf::from))?;
    match cli.command {
        Command::List { state } => list(&open_for_read(&root)?, state, cli.json),
        Command::Show { id } => show(&open_for_read(&root)?, id, cli.json),
        Command::Create { title, description, state, tags } => {
            create(&open(&root)?, title, description, state, tags, cli.json)
        }
        Command::Delete { id, force } => delete(&open_for_read(&root)?, id, force, cli.json),
        Command::Move { id, state } => move_task(&open_for_read(&root)?, id, state, cli.json),
        Command::Tag { id, name } => tag(&open_for_read(&root)?, id, name, cli.json),
        Command::Untag { id, name } => untag(&open_for_read(&root)?, id, name, cli.json),
    }
}

/// `--root` verbatim (the directory must exist), else the nearest ancestor
/// of the current directory containing `zedgg.sqlite`.
fn resolve_root(root: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(root) = root {
        anyhow::ensure!(root.is_dir(), "--root {} is not a directory", root.display());
        return Ok(root);
    }
    let start = std::env::current_dir().context("reading current directory")?;
    let mut dir = start.as_path();
    loop {
        if dir.join(DB_FILE).is_file() {
            return Ok(dir.to_path_buf());
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => bail!(
                "no {DB_FILE} found in {} or any parent directory (pass --root, or run \
                 a create command with --root to start one)",
                start.display()
            ),
        }
    }
}

/// Read and mutate-existing commands never create the database.
fn open_for_read(root: &std::path::Path) -> Result<Connection> {
    open_existing(root)?
        .with_context(|| format!("no {DB_FILE} in {} (create a task first)", root.display()))
}

fn tags_by_id(connection: &Connection) -> Result<Vec<Tag>> {
    tasks::list_tags(connection)
}

fn tag_names(all_tags: &[Tag], tag_ids: &[i64]) -> Vec<String> {
    tag_ids
        .iter()
        .filter_map(|id| all_tags.iter().find(|tag| tag.id == *id))
        .map(|tag| tag.name.clone())
        .collect()
}

fn find_tag<'a>(all_tags: &'a [Tag], name: &str) -> Option<&'a Tag> {
    all_tags.iter().find(|tag| tag.name.eq_ignore_ascii_case(name))
}

fn require_task(connection: &Connection, id: i64) -> Result<tasks::TaskRow> {
    tasks::get_task(connection, id)?.with_context(|| format!("no task with id {id}"))
}

fn list(connection: &Connection, state: Option<String>, json: bool) -> Result<()> {
    let filter = state.map(|state| TaskState::parse(&state)).transpose()?;
    let all_tags = tags_by_id(connection)?;
    let rows: Vec<_> = tasks::list_tasks(connection)?
        .into_iter()
        .filter(|task| filter.is_none_or(|state| task.state == state))
        .collect();
    if json {
        let out: Vec<_> = rows
            .iter()
            .map(|task| {
                json!({
                    "id": task.id,
                    "state": task.state.as_str(),
                    "title": task.title,
                    "tags": tag_names(&all_tags, &task.tag_ids),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    for state in TaskState::ALL {
        if filter.is_some_and(|filter| filter != state) {
            continue;
        }
        let in_state: Vec<_> = rows.iter().filter(|task| task.state == state).collect();
        println!("{} ({})", state.label(), in_state.len());
        for task in in_state {
            let tags = tag_names(&all_tags, &task.tag_ids).join(", ");
            if tags.is_empty() {
                println!("  #{} {}", task.id, task.title);
            } else {
                println!("  #{} {} [{}]", task.id, task.title, tags);
            }
        }
    }
    Ok(())
}

fn show(connection: &Connection, id: i64, json: bool) -> Result<()> {
    let task = require_task(connection, id)?;
    let all_tags = tags_by_id(connection)?;
    let tags = tag_names(&all_tags, &task.tag_ids);
    let description = tasks::load_description(connection, id)?;
    let files: Vec<String> = tasks::list_files(connection, id)?
        .into_iter()
        .map(|file| file.name)
        .collect();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "id": task.id,
                "state": task.state.as_str(),
                "title": task.title,
                "tags": tags,
                "description": description,
                "files": files,
            }))?
        );
        return Ok(());
    }
    println!("#{} {}", task.id, task.title);
    println!("state: {}", task.state.label());
    if !tags.is_empty() {
        println!("tags: {}", tags.join(", "));
    }
    if !files.is_empty() {
        println!("files: {}", files.join(", "));
    }
    if !description.is_empty() {
        println!("\n{description}");
    }
    Ok(())
}

fn create(
    connection: &Connection,
    title: String,
    description: Option<String>,
    state: Option<String>,
    tags: Vec<String>,
    json: bool,
) -> Result<()> {
    let state = state.map(|state| TaskState::parse(&state)).transpose()?;
    let id = tasks::create_task(connection, &title)?;
    if let Some(description) = description {
        tasks::save_description(connection, id, &description)?;
    }
    if let Some(state) = state
        && state != TaskState::Backlog
    {
        tasks::move_task_between(connection, id, state, None, None)?;
    }
    for name in tags {
        assign_by_name(connection, id, &name)?;
    }
    if json {
        println!("{}", json!({ "id": id }));
    } else {
        println!("created #{id}");
    }
    Ok(())
}

fn delete(connection: &Connection, id: i64, force: bool, json: bool) -> Result<()> {
    let task = require_task(connection, id)?;
    if !force {
        use std::io::IsTerminal as _;
        if !std::io::stdin().is_terminal() {
            bail!("refusing to delete #{id} without --force (stdin is not a terminal)");
        }
        eprint!(
            "Delete #{} {:?} and its tags assignments and attachments? [y/N] ",
            id, task.title
        );
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).context("reading confirmation")?;
        if !answer.trim().eq_ignore_ascii_case("y") {
            bail!("aborted");
        }
    }
    tasks::delete_task(connection, id)?;
    if json {
        println!("{}", json!({ "deleted": id }));
    } else {
        println!("deleted #{id}");
    }
    Ok(())
}

fn move_task(connection: &Connection, id: i64, state: String, json: bool) -> Result<()> {
    let state = TaskState::parse(&state)?;
    tasks::move_task_between(connection, id, state, None, None)?;
    if json {
        println!("{}", json!({ "id": id, "state": state.as_str() }));
    } else {
        println!("moved #{id} to {}", state.label());
    }
    Ok(())
}

fn assign_by_name(connection: &Connection, id: i64, name: &str) -> Result<i64> {
    let name = name.trim();
    anyhow::ensure!(!name.is_empty(), "tag name may not be empty");
    let tag_id = match find_tag(&tags_by_id(connection)?, name) {
        Some(tag) => tag.id,
        None => tasks::create_tag_with_next_color(connection, name, &TAG_COLORS)?,
    };
    tasks::assign_tag(connection, id, tag_id)?;
    Ok(tag_id)
}

fn tag(connection: &Connection, id: i64, name: String, json: bool) -> Result<()> {
    require_task(connection, id)?;
    let tag_id = assign_by_name(connection, id, &name)?;
    if json {
        println!("{}", json!({ "id": id, "tag": name, "tag_id": tag_id }));
    } else {
        println!("tagged #{id} with {name}");
    }
    Ok(())
}

fn untag(connection: &Connection, id: i64, name: String, json: bool) -> Result<()> {
    require_task(connection, id)?;
    let all_tags = tags_by_id(connection)?;
    let tag = find_tag(&all_tags, &name).with_context(|| format!("no tag named {name:?}"))?;
    tasks::unassign_tag(connection, id, tag.id)?;
    if json {
        println!("{}", json!({ "id": id, "untagged": tag.name }));
    } else {
        println!("untagged {} from #{id}", tag.name);
    }
    Ok(())
}
