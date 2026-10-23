# Architecture

```
                 ┌────────────────────────── bettergrep daemon ──────────────────────────┐
 NTFS ─────────► │ watcher threads (ReadDirectoryChangesExW, one per root) ─┐          │
                 │                                                          ├─► channel ─► main thread
 Registry ─────► │ registry thread (RegNotifyChangeKeyValue + snapshot/diff) ┘          │   Tracker ─► SQLite ─► .bettergrep.json
                 └─────────────────────────────────────────────────────────────────────┘
 bettergrep CLI (where, organize, mv, undo, scan…) ──┐
 bettergrep mcp (stdio JSON-RPC)                     ├── read the same database (WAL); explicit operations write
 bettergrep trace (own watchers + parallel registry) ┘
```

| Module | Responsibility |
|---|---|
| `store.rs` | Versioned schema, queries, depth-counted transactions |
| `tracker.rs` | Lineage engine: settle, held-back deletions, matching, following by ID, reconcile |
| `watcher.rs` | `ReadDirectoryChangesExW` with extended info: folder flag on every event, overflow detection, fallback for older volumes |
| `win.rs` | Small safe wrappers: file IDs, `OpenFileById`, bulk directory listing, hidden attribute |
| `registry.rs` | Handle-relative snapshot, parallel snapshot, diff, notification thread |
| `daemon.rs` | Event loop, heartbeat (`daemon.json`), stop file, single-instance lock, config reload, periodic rescan |
| `folder_index.rs` | Builds and atomically writes `.bettergrep.json` |
| `operations.rs` | Organize, move and undo, with operation ids, actors and reasons |
| `trace.rs` | Trace sessions, background learning, summaries, UAC launch, Markdown report |
| `query.rs` | Read API shared by the CLI, the MCP server and the indexes |
| `mcp.rs` | MCP over stdio |

## Lineage rules

A row in `files` is one logical file, and its `id` never changes while bettergrep can follow it. When something appears at a path, the tracker checks these in order:

1. **Known path.** Refresh the record. `modified` is written only if size or SHA-256 changed. If that record was pending deletion, this was a replace-in-place save: identity is kept and the event carries `replaced: true`.
2. **Same NTFS file ID** (same volume) as a tracked file that is no longer at its old path: a move.
3. **Pending deletion with the same content** (same name and size, or same SHA-256): a move across volumes or between watched trees.
4. **Same content as a file that still exists**: a copy. If the original is deleted within the move window, the copy folds into the original and the event becomes `moved`. This is how Explorer moves across drives.
5. Otherwise a new file: `created` (live) or `discovered` (found by a scan).

Deletions are held back for `move_window_seconds` so that steps 2–3 can claim them. A deletion nobody claimed is then **followed**: `OpenFileById` with the stored file ID, then `GetFinalPathNameByHandleW`. The result decides what is recorded:

- The file is found elsewhere: `moved` to that path, with `details.matched = "file_id_lookup"`.
- It is under `\$Recycle.Bin\`: `deleted`, with `recycled: true` and its path in the bin.
- It is not found: `deleted`.

The same lookup runs during scans for files that vanished while the daemon was off. Only files that truly no longer exist become `missing`.

New files *settle* for `settle_seconds` before they are hashed, so half-written downloads are never fingerprinted. A folder that appears in one piece (moved in, or extracted) is *adopted*: its subfolders are recorded at once and its files settle like any new file.

## Threads and safety

- Each watcher thread owns its directory handle and an overlapped read. Stopping sets a shared event; the thread cancels its read and exits.
- Notification records are only 4-byte aligned while the record structs contain 8-byte fields, so headers are copied out with `read_unaligned`. Debug builds catch misaligned references, and the live tests run in debug mode.
- The main thread is the only database writer in the daemon. The CLI and MCP server write only for explicit operations. They move the file first, then record it. If the daemon recorded the move in between, they annotate its event instead of writing a duplicate.

## Registry walk

`RegNotifyChangeKeyValue` only reports *that* a subtree changed, so bettergrep diffs snapshots. Three things keep this fast:

- Each child key is opened **relative to its open parent**, instead of resolving the full path from the hive root every time. That alone cut a full baseline by about 30%.
- Traces read the top two levels first, then spread the subtrees over a work queue across up to 8 threads. Registry reads scale across threads, so a ~500k-key baseline takes ~8 s instead of ~16 s.
- A key's cached values are reused when its last-write time is unchanged *and* that time was more than 2 s before the cached read. Last-write times only move on clock ticks, so a write in the same tick as the previous read would otherwise be missed.

Key names may contain `/` (MIME types such as `application/pdf`), so only `\` separates key paths.

## Data formats and versioning

The database schema (`user_version = 1`), `config.json` and `.bettergrep.json` (`bettergrep.folder-index/1`) are versioned. New fields are always optional, so older readers ignore them. The daemon keeps `daemon.json`, `stop.request` and `daemon.lock` in the data directory, so the CLI, the MCP server and scripts can all read the same data, and two daemons can never run at the same time on one data directory.
