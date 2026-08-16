# zedgg-tasks — agent usage

CLI for the ZedGG task board stored in a project's `zedgg.sqlite` (the
same data the IDE's Tasks panel and board edit; the GUI picks up your
writes automatically). Use it to read, create, and update tasks from
scripts and agent sessions.

## Invocation

Installed binary: `zedgg-tasks`. From this repo without installing:

```
cargo run -q -p zedgg_tasks_cli -- <args>
```

## Rules

- **Always pass `--json`.** Human output is unstable; JSON is the
  contract. JSON goes to stdout; in `--json` mode errors are
  `{"error": "..."}` on stderr.
- **Always pass `--force` with `delete`.** Without a TTY, `delete`
  refuses rather than prompting.
- **Address tasks by id, never by title.** Ids are stable; titles are
  user-editable.
- Exit code 0 = success, 1 = failure (message on stderr). No partial
  success: a failed command changed nothing except where a multi-step
  `create` (description/state/tags) names the failing step in its error.
- The database is found by walking up from the current directory to the
  nearest `zedgg.sqlite`; run commands from anywhere inside the project.
  `--root <dir>` overrides. Only write commands with an explicit
  `--root` may create a fresh database; reads never create anything.

## States

`backlog` | `in_progress` | `review` | `done` — always these strings.

## Commands

```
zedgg-tasks --json list [--state <state>]
  → [{"id": 7, "state": "backlog", "title": "...", "tags": ["art"]}, ...]
    Ordered by state column order, then board rank.

zedgg-tasks --json show <id>
  → {"id": 7, "state": "review", "title": "...", "tags": [...],
     "description": "markdown body", "files": ["shot.png"]}

zedgg-tasks --json create <title> [-d <markdown>] [--state <state>] [--tag <name>]...
  → {"id": 8}
    New tasks land at the top of Backlog unless --state says otherwise.
    Unknown --tag names are created (colors auto-assigned).

zedgg-tasks --json delete <id> --force
  → {"deleted": 8}
    Cascades the task's tag assignments and attachments.

zedgg-tasks --json move <id> <state>
  → {"id": 7, "state": "done"}
    Lands at the end of the target column.

zedgg-tasks --json tag <id> <name>      → {"id": 7, "tag": "art", "tag_id": 3}
zedgg-tasks --json untag <id> <name>    → {"id": 7, "untagged": "art"}
    Tag names are matched case-insensitively; unknown names are created
    by `tag`, rejected by `untag`.
```

## Not covered here

Attachments (import via the IDE task tab), board rank reordering beyond
move-to-end, and description editing beyond `create -d` (open the task
in ZedGG to edit markdown with preview).
