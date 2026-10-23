# Changelog

All notable changes to this project are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/).

## [0.1.0] - 2026-10-23

First public release.

### Added

- Lineage engine: every file in a tracked folder keeps one identity through renames, moves, copies and deletions, and can be looked up by a former name.
- Files that leave the tracked folders are followed by NTFS file ID (`OpenFileById`), so they are recorded as moved, with their new location, instead of deleted. Files sent to the Recycle Bin are recorded as recycled, with their path in the bin.
- `ReadDirectoryChangesExW` watcher, with a folder flag on every event. Lost notifications trigger an immediate rescan, and older volumes fall back to `ReadDirectoryChangesW`.
- Bulk directory scans with `FileIdBothDirectoryInfo`.
- Registry snapshots that open each key relative to its parent, plus a parallel walk for trace baselines: about 8 s for ~500k keys.
- Trace sessions (`bettergrep trace`) that record what a program changed on disk and in the registry, and warn when notifications were dropped.
- Embedded application manifest: long-path aware, UTF-8 code page, `asInvoker`.
- `bettergrepw.exe`, a windowless build used for autostart.
- MCP server with 17 tools, including `find_anywhere` (full-drive search by name) and `grep_files` (search inside files).
- Full-drive mode (`bettergrep drives`, `bettergrep find`): every file on an NTFS volume, read from the master file table and kept current from the USN change journal. Each volume's index is a snapshot file that is mapped into memory and searched in place, plus a small overlay of live changes that is written to a side file every five minutes, so readers without administrator rights (CLI, MCP) are never far behind. The process holds about 2 MB of its own memory for two drives of ~4 million files each, and Windows can drop the mapped pages under memory pressure without using the page file.
- The daemon keeps full-drive indexes current from each volume's change journal when it runs with administrator rights. `bettergrep autostart enable` from an administrator terminal registers it as a logon task with highest privileges (no time limit, keeps running on battery); from a normal terminal it uses the per-user `Run` key as before.
- `bettergrep drives add C:` indexes the drive straight away when run as administrator, and says what to do when it is not.
- `bettergrep grep`: search inside files using ripgrep's libraries. Folders are walked in parallel and respect `.gitignore`; drive roots in full-drive mode are listed from the index instead of walked. Files with binary extensions are skipped without being opened. Globs (`-g`) are case-insensitive, like Windows file names.
- `bettergrep PATTERN [PATHS]` is short for `bettergrep grep PATTERN [PATHS]`.
- `bettergrep grep` shows progress on stderr during long searches ("searching... 12,000 files, 3 matches, 40 s"), so a search reading a cold disk does not look hung. On a console it is one line updated in place; when stderr is captured it is a plain line every ten seconds. Not shown with `--json`.
- `bettergrep find --limit 0` means no limit, as it does for `bettergrep grep`.
- Activity feed: repeated creations, deletions and edits of one file within a minute fold into a single entry with a count ("Rewritten 114x"), so a program that rewrites a settings file several times a second cannot flood the feed or push real events out of it. Folders can be muted from the feed (`activity_mute` in `config.json`); their changes are still recorded and searchable.

[0.1.0]: https://github.com/affifuddin-hazam/bettergrep/releases/tag/v0.1.0
