<div align="center">

# bettergrep

**grep and find for Windows that already know where every file is.**

Search inside files with ripgrep's engine, find any file on your drives by name in under 100 ms,
and ask where a file went after it was renamed or moved. One 7 MB executable, no runtime.

[![CI](https://github.com/affifuddin-hazam/bettergrep/actions/workflows/ci.yml/badge.svg)](https://github.com/affifuddin-hazam/bettergrep/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/affifuddin-hazam/bettergrep)](https://github.com/affifuddin-hazam/bettergrep/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Windows 10/11](https://img.shields.io/badge/platform-Windows%2010%20%7C%2011-0078d4)

<img src="assets/demo.svg" alt="bettergrep finding docker.exe among 9.8 million files in 73 ms, searching source code, and showing the trail of a renamed and moved PDF" width="900">

</div>

## What it does

- **`bettergrep PATTERN [PATH]`** searches inside files, ripgrep-style: regex or `-F` plain text, `-i`, `-l`, `-g "*.rs"`, `.gitignore` respected, binary files skipped. Same results as ripgrep, about twice as fast on the benchmark below.
- **`bettergrep find NAME`** finds any file or folder on your NTFS drives by name. It reads each drive's master file table once, follows the USN change journal after that, and searches 9.8 million names in 40 to 85 ms with about 5 MB of memory.
- **`bettergrep TODO C:\`** greps a whole drive. The file list comes from that same index, so it skips walking millions of folders.
- **`bettergrep where FILE`** tells you where a file is now, by its current name, a former name, or any path it ever had. Renames and moves are followed by NTFS file ID, even into folders nobody is watching. Files sent to the Recycle Bin are recorded as recycled, with their path in the bin.
- **`bettergrep trace -- setup.exe`** records every file and registry value an installer creates, changes or deletes, including temporary files it cleans up, and writes a readable report.
- **`bettergrep mcp`** exposes all of this to Claude, Cursor, VS Code and other MCP clients, so an assistant can find files on your PC without crawling your disk.

## Install

Download `bettergrep-x86_64-pc-windows-msvc.zip` from the [latest release](https://github.com/affifuddin-hazam/bettergrep/releases/latest), unzip it somewhere on your `PATH`, and run `bettergrep --help`. The zip holds `bettergrep.exe` and `bettergrepw.exe`, the same program without a console window, which autostart uses. The release page lists SHA-256 checksums.

Or build it with Rust 1.89 or later:

```powershell
cargo install --git https://github.com/affifuddin-hazam/bettergrep
```

Requires Windows 10 1709 or later.

## Quick start

Search inside files. Nothing to set up:

```powershell
bettergrep "connection string" C:\work -i -g "*.config"
bettergrep -l TODO .                      # just the file names
bettergrep -F "unsafe impl" src --json    # machine-readable
```

Find files by name on whole drives. Indexing reads the master file table, which needs an administrator terminal once:

```powershell
bettergrep drives add C: D:               # as administrator: index the drives
bettergrep autostart enable               # as administrator: keep them current at every logon
bettergrep find invoice 2026              # from any terminal
bettergrep find .psd --limit 0            # every match
```

`find` itself needs no administrator rights. With the monitor running as administrator, it saves the drives' changes every five minutes, so a search from a normal terminal is never far behind.

Track file history:

```powershell
bettergrep init                           # track your Downloads folder
bettergrep where tax-return-2026.pdf
bettergrep changes --since 2h
bettergrep trace --report setup.md -- setup.exe /S
```

Every command accepts `--json`. Data lives in `%LOCALAPPDATA%\bettergrep`; use `--home DIR` or `BETTERGREP_HOME` to put it elsewhere.

## Benchmarks

Searching the Cargo registry source cache: 599 crates, 29,752 files, 912 MB. Warm file cache, every matching line printed, median of 5 runs (3 for `findstr`, 1 for `Select-String`).

| Tool | literal `unsafe impl Send` | regex `fn \w+_unchecked\(` | `-i -l deprecated` | no match |
|---|---:|---:|---:|---:|
| **bettergrep 0.1.0** | **651 ms** | **607 ms** | **622 ms** | **636 ms** |
| ripgrep 15.1.0 | 1,403 ms | 1,387 ms | 1,421 ms | 1,567 ms |
| `findstr /s` | 4,588 ms | | | |
| GNU grep 3.0 (`grep -rnI`, from Git for Windows) | 6,981 ms | 6,804 ms | 6,695 ms | 6,769 ms |
| PowerShell `Select-String` | 21,696 ms | | | |

All tools found the same 7,345 lines for the literal search, and bettergrep and ripgrep agreed in every column (414 lines, 2,412 files). GNU grep listed 2,414 files for `-i -l` because it also searches hidden `.github` folders, which ripgrep and bettergrep skip unless given `--hidden`. The two built-in Windows tools were measured on the literal search only.

By default bettergrep skips files with binary extensions (`.dll`, `.lib`, `.a`, images, archives) without opening them; ripgrep opens them to check. That is not where the difference comes from: with `--binary`, bettergrep opens all 29,752 files and the literal search takes 687 ms.

Finding files by name, across four NTFS drives with 9,799,761 entries in total:

| Query | Matches | Search time |
|---|---:|---:|
| an exact file name | 7 | 40 ms |
| `invoice 2026` (two terms) | 3 | 45 ms |
| `.psd` | 3,414 | 49 ms |
| `a` (one letter) | 5,748,110 | 77 ms |

The process holds 5 MB of private memory with all four indexes open. The indexes are memory-mapped files, so Windows can drop their pages under pressure without touching the page file. A full `bettergrep find` run, including process start, takes about 160 ms.

Machine: AMD Ryzen 5 3600, PNY CS3030 NVMe SSD, Windows 10 22H2, ESET real-time protection on. Your numbers will differ, so measure on your own files:

```powershell
.\scripts\bench.ps1 -Corpus "$env:USERPROFILE\.cargo\registry\src"
```

## How is this different from...

**ripgrep.** bettergrep uses ripgrep's own libraries (`grep-searcher`, `grep-regex`, `ignore`) for searching, so patterns and results match. It adds what a Windows machine needs around them: a drive-wide name index, grep over whole drives without walking them, file history, and an MCP server. If you only grep source trees, ripgrep is excellent and cross-platform; bettergrep is Windows-only.

**Everything (voidtools).** Everything is the best-known instant file search on Windows and also reads the master file table. It is a GUI with an optional command-line client. bettergrep is a command-line tool first, with JSON output and an MCP server, and combines name search with content search and file history in one executable.

**`findstr` / `Select-String` / `dir /s`.** These walk every folder and read every file on each run. bettergrep's `find` never walks the disk after the first index.

## How it works

- **Name index.** `FSCTL_ENUM_USN_DATA` lists every file and folder on a volume with its parent, without opening a directory. The result is saved as a compact snapshot that is memory-mapped and searched in place by all cores. After that, `FSCTL_READ_USN_JOURNAL` applies only what changed, including changes made while bettergrep was not running.
- **Content search.** Folders are walked in parallel with ripgrep's `ignore` crate. Whole drives are listed from the index instead. Each file is searched with `grep-searcher`, and a file is treated as binary and skipped at its first NUL byte, like ripgrep.
- **File history.** A monitor watches the folders you track with `ReadDirectoryChangesExW` and records every file in SQLite. When a file disappears, it asks Windows where that NTFS file ID lives now (`OpenFileById`) before calling it deleted. Each tracked folder also gets a small `.bettergrep.json` listing what is there, where it came from, and where departed files went.
- **Installer traces.** Registry snapshots open each key relative to its parent and walk hives in parallel: about 8 s for 500,000 keys. Files are watched for the duration of the trace.

Details: [architecture](docs/architecture.md), [folder index format](docs/folder-index.md), [database schema](docs/schema.md), [tracing](docs/tracing.md), [MCP tools](docs/mcp.md).

## Use it from an AI assistant

```json
{ "mcpServers": { "bettergrep": { "command": "bettergrep", "args": ["mcp"] } } }
```

With Claude Code: `claude mcp add bettergrep -- bettergrep mcp`. Add `--read-only` to hide the tools that move files. The 17 tools are listed in [docs/mcp.md](docs/mcp.md).

## Commands

| Command | Purpose |
|---|---|
| `[grep] <pattern> [paths]` | Search inside files (`-i`, `-F`, `-l`, `-g`, `--drives`, `--limit`, `--timeout`). `grep` can be left out |
| `find <terms>` | Find files and folders by name on indexed drives |
| `drives [list\|add\|remove\|scan]` | Manage the drive indexes (indexing needs administrator rights) |
| `where` / `history <file>` | Where a tracked file is now / its full history |
| `search <terms>` | Search tracked files (`--status deleted`, `--under`, `--type`) |
| `changes --since 2h` | Timeline of file and registry changes |
| `init [folders]` | Track folders (default: Downloads) |
| `roots [list\|add\|remove]` | Manage tracked folders |
| `daemon` / `stop` / `status` | Run, stop or inspect the background monitor |
| `autostart enable\|disable\|status` | Start the monitor at logon |
| `scan [folders]` | Catch up on changes made while the monitor was off |
| `registry --since 1d` | Registry changes seen by the monitor |
| `trace [-- cmd ...]` | Record what a program changes on disk and in the registry |
| `session start\|stop\|show`, `sessions` | Background trace sessions |
| `organize <folder> [--apply]` | Sort loose files into category folders; `undo` reverts it |
| `mv`, `undo`, `ops` | Moves with a recorded reason, and undo |
| `index <folder> [--write]` | Print or write a folder's `.bettergrep.json` |
| `export` | Dump everything as JSON Lines |
| `mcp` | MCP server on stdio |

## Privacy

Everything stays on your PC. bettergrep makes no network connections and has no telemetry. See [SECURITY.md](SECURITY.md) for what is stored, including the note about `.bettergrep.json` files in synced folders.

## Limitations

- Windows only, NTFS for drive indexes. FAT32 and exFAT drives can still be searched by walking them.
- Indexing a drive and keeping it current need administrator rights. Without them, `find` uses the last saved index.
- Following files by ID works within one volume. A file moved to another drive, outside the tracked folders, is recorded as deleted.
- Trace attribution is by time window, not by process. Background activity is learned before launch and reported separately.

## Building

```powershell
cargo build --release
cargo test                                  # includes live tests with real notifications and registry keys
cargo clippy --all-targets -- -D warnings
```

If the checkout path is long, set `CARGO_TARGET_DIR` to a short path such as `%TEMP%\bettergrep-target`, because the MSVC linker cannot write paths over 260 characters. Contributions are welcome; see [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[MIT](LICENSE)
