# Database schema

The database lives at `%LOCALAPPDATA%\bettergrep\bettergrep.db` (or `$BETTERGREP_HOME/bettergrep.db`). It is SQLite in WAL mode, so it is safe to read while the daemon runs. Open it read-only from other tools:

```bash
sqlite3 "file:%LOCALAPPDATA%/bettergrep/bettergrep.db?mode=ro" "SELECT path, status FROM files WHERE name LIKE '%invoice%'"
```

Times are Unix epoch seconds (`REAL`). `*_key` columns are case-folded paths used for lookups. Compare against them, not against `path`. The schema version is in `PRAGMA user_version`, currently `1`.

## `files`: one row per logical file or folder

| Column | Notes |
|---|---|
| `id` | Stable identity, kept across renames and moves. |
| `path`, `name` | Current path, or last known path for deleted and missing files. |
| `path_key`, `parent_key` | Case-folded full path and parent folder. |
| `is_dir` | 1 for folders. |
| `size`, `mtime`, `sha256` | `sha256` is NULL until hashed (files above `hash_max_bytes` are never hashed). |
| `volume`, `inode` | Volume serial and NTFS file ID at the last observation. |
| `status` | `present`, `deleted` or `missing`. |
| `copied_from` | `files.id` of the file this one was copied from. |
| `first_seen`, `last_seen` | |

Only one `present` row can exist per `path_key`. This is enforced by a partial unique index.

## `events`: append-only timeline

| Column | Notes |
|---|---|
| `ts`, `kind`, `source` | `source`: `watcher`, `scan`, `organizer`, `cli`, `mcp`, `registry`. |
| `file_id` | The file the event is about (NULL for registry events). |
| `path` / `path_key` / `folder_key` | Subject path. For moves and copies this is the **source**. For registry events it is the key path. |
| `dest_path` / `dest_key` / `dest_folder_key` | Destination for `moved`, `renamed` and `copied`. |
| `related_id` | Copy source, or the file that overwrote this one. |
| `op_id`, `actor`, `reason` | Set for recorded operations (organize, mv, undo, MCP). |
| `details` | JSON object, e.g. `{"matched": "file_id"}`, `{"matched": "file_id_lookup", "outside_tracked_folders": true}` (followed by ID, Rust), `{"recycled": true, "recycle_bin_path": "…"}` (Rust), `{"replaced": true}`, registry `{"value", "type", "old", "new"}`. |

File kinds: `discovered`, `created`, `modified`, `renamed`, `moved`, `copied`, `deleted`, `missing`.
Registry kinds: `reg_key_created`, `reg_key_deleted`, `reg_value_created`, `reg_value_changed`, `reg_value_deleted`.

## `operations`

`id` (the `op_id`), `ts`, `kind` (`organize`, `move` or `undo`), `actor`, `description`, `undone_by`.

## `sessions` and `session_events`

Trace sessions. `sessions.summary` is the JSON summary shown by `bettergrep session show`. `session_events` has every raw file event (`source = 'fs'`) and registry change (`source = 'registry'`) recorded during the session.

## Useful queries

```sql
-- Where is every file that used to be in Downloads?
SELECT e.path AS was, f.path AS now, f.status
FROM events e JOIN files f ON f.id = e.file_id
WHERE e.folder_key = lower('C:\Users\me\Downloads') AND e.kind = 'moved';

-- What appeared in autostart this week?
SELECT datetime(ts, 'unixepoch', 'localtime'), path, details
FROM events WHERE kind LIKE 'reg_value_%' AND path_key LIKE '%\run%' AND ts > strftime('%s', 'now', '-7 days');
```
