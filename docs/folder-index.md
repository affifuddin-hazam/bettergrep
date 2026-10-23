# Folder index format (`.bettergrep.json`)

Every folder inside a root with `folder_index: true` gets a hidden `.bettergrep.json` when its history changes. The file is a projection of the database. It is meant to be read by people, scripts and AI tools without installing anything, and it is safe to delete, since bettergrep regenerates it.

Schema id: `bettergrep.folder-index/1`. All times are local ISO-8601 with offset. All paths are absolute. Fields that would be `null` are omitted.

```json
{
  "schema": "bettergrep.folder-index/1",
  "generator": "bettergrep 0.1.0",
  "folder": "C:\\Users\\me\\Downloads",
  "summary": { "files_here": 1, "folders_here": 5, "departed": 7, "copied_out": 0, "deleted": 2, "truncated": false },
  "contents": [
    { "id": 3, "name": "mystery.xyz", "type": "file", "size": 23, "modified": "…", "arrived": "…", "how": "discovered" }
  ],
  "departed": [
    {
      "id": 7, "name": "tax-2026.pdf", "left": "2026-10-19T08:55:32+08:00",
      "moved_to": "C:\\Users\\me\\Downloads\\Documents\\tax-2026.pdf",
      "now_at":   "C:\\Users\\me\\Documents\\Taxes\\tax-2026.pdf",
      "now_status": "present", "by": "bettergrep organize", "reason": "category: Documents"
    }
  ],
  "copied_out": [],
  "deleted": [ { "id": 14, "name": "old.log", "type": "file", "status": "deleted", "when": "…", "size": 1200 } ],
  "recent_events": [ … ],
  "help": "…"
}
```

## Sections

### `contents`: what is here now

| Field | Meaning |
|---|---|
| `id` | Stable bettergrep file id. Use it with `bettergrep history <id>` or the `file_history` tool. |
| `name`, `type`, `size`, `sha256`, `modified` | Current metadata. `sha256` is present once the file has been hashed. |
| `arrived`, `how` | When and how it got here: `moved`, `copied`, `created` or `discovered` (already present when bettergrep first looked). |
| `came_from` | Previous full path, for files moved or copied in from another folder. |
| `by`, `reason` | Who moved it and why, when recorded (for example `bettergrep organize`, `category: Images`). |
| `previous_name` | Set if the file was renamed inside this folder. |

### `departed`: files that were moved out

| Field | Meaning |
|---|---|
| `moved_to` | Where it went when it left this folder. |
| `now_at` | Where it is **today**. If it moved again later, this differs from `moved_to`. Absent if the file no longer exists. |
| `now_status` | `present`, `deleted` or `missing`. |

A file that later comes back to this folder is removed from `departed` and appears in `contents`.

### `copied_out`

Copies of files from this folder that were created elsewhere: `copy_path` and `copy_id`.

### `deleted`

Files last seen in this folder. `status: deleted` means the deletion was observed live. `status: missing` means a scan found the file gone while the daemon was off.

bettergrep looks a vanished file up by its NTFS file ID before calling it deleted. Files moved somewhere untracked therefore show up under `departed`, with their new location. Files sent to the Recycle Bin stay under `deleted`, with an extra `recycle_bin_path` field.

### `recent_events`

The last 50 events in which this folder was the source or the destination, in the same format as `bettergrep changes --json`.

## Limits

Each list is capped at `index_max_entries` (default 500). When `contents` is cut short, `summary.truncated` is set. The database always keeps the full history.
