# MCP server

`bettergrep mcp` speaks the [Model Context Protocol](https://modelcontextprotocol.io) over stdio: newline-delimited JSON-RPC 2.0, protocol versions `2025-06-18`, `2025-03-26` and `2024-11-05`. It reads the same database as the daemon, so answers reflect live state when the daemon is running.

## Client configuration

**Claude Code**

```bash
claude mcp add bettergrep -- bettergrep mcp
```

**Claude Desktop, Cursor and other JSON-configured clients**

```json
{
  "mcpServers": {
    "bettergrep": { "command": "bettergrep", "args": ["mcp"] }
  }
}
```

If `bettergrep` is not on `PATH`, use the full path to `bettergrep.exe`.

**VS Code** (`.vscode/mcp.json`)

```json
{ "servers": { "bettergrep": { "type": "stdio", "command": "bettergrep", "args": ["mcp"] } } }
```

Options: `--read-only` hides every tool that changes files or starts sessions. `--home DIR` (before `mcp`) selects a different data directory.

## Tools

Read-only (`readOnlyHint: true`):

| Tool | Arguments | Returns |
|---|---|---|
| `find_anywhere` | `query`, `type?`, `hidden?`, `limit?` | Any file on the indexed drives (full-drive mode), with `as_of` per drive |
| `grep_files` | `pattern`, `path?`, `glob?`, `ignore_case?`, `fixed?`, `files_only?`, `hidden?`, `limit?`, `timeout_seconds?` | Matching lines (path, line, text), with `complete` and how each path was listed |
| `locate_file` | `target`: id, name, current **or former** path | Matches with `current_path` and the full `trail` |
| `file_history` | `target`, `limit?` | File, trail, copies, origin, events |
| `folder_index` | `folder`, `limit?` | Same structure as `.bettergrep.json` |
| `search_files` | `query`, `status?`, `under?`, `type?`, `limit?` | File records |
| `recent_changes` | `since?`, `kinds?`, `under?`, `source?`, `limit?` | Events, newest first |
| `registry_changes` | `since?`, `key_prefix?`, `limit?` | Registry events |
| `list_trace_sessions` | `limit?` | Sessions with per-kind counts |
| `trace_session_report` | `session_id`, `include_events?`, `source?`, `kind?`, `under?`, `limit?` | Summary (+ raw events) |
| `list_operations` | `limit?` | Recent operations and their `op_id` |
| `bettergrep_status` | | Roots, daemon state, DB statistics |

Changing (`readOnlyHint: false`, never destructive: nothing is ever deleted):

| Tool | Notes |
|---|---|
| `organize_folder` | `apply` defaults to `false`. Show the plan to the user before applying. |
| `move_file` | `src`, `dest` (file path or existing folder), `reason`. Refuses to overwrite. |
| `undo_operation` | Reverts an `op_id`: moves files back and removes folders it created, if they are empty. |
| `start_trace_session` | Starts a detached recorder (about 8 s registry baseline in `full` mode). |
| `stop_trace_session` | Stops the latest (or given) session and returns its summary. |

Each result is returned twice: as `structuredContent` (a JSON object) and as pretty-printed JSON text, for clients that only read text. Failures set `isError: true` with an `error` message.

## Server instructions

`initialize` returns `instructions` that tell the model when to use which tool. In particular, the model should route file moves through `move_file` or `organize_folder`, so they are recorded with a reason and can be undone.
