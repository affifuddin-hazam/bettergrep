# Security and privacy

## What bettergrep stores

Everything stays local, in `%LOCALAPPDATA%\bettergrep` (or `BETTERGREP_HOME`):

- the full paths, sizes and SHA-256 hashes of files in tracked folders, and their history;
- registry keys and values seen by the monitor and by trace sessions. **Values can contain secrets**, for example tokens in environment variables or credentials stored by badly behaved apps. Strings are stored up to 512 characters;
- `.bettergrep.json` index files inside tracked folders. These live next to your files, so **anything that syncs or shares those folders shares the index too** (for example OneDrive, Dropbox or a network share). Use `bettergrep roots add <folder> --no-index` for such folders.

bettergrep makes no network connections and has no telemetry.

## Permissions

- The daemon runs as your user. It sees only what you can see, and autostart uses the per-user `Run` key.
- `bettergrep trace` asks for elevation only when the traced program itself requires it, through the standard UAC prompt.
- The MCP server exposes file-moving tools by default. Run `bettergrep mcp --read-only` if the client shouldn't be able to move files. No tool deletes files.

## Reporting a vulnerability

Please don't open a public issue. Use GitHub's private vulnerability reporting ("Security" → "Report a vulnerability") on the repository, with steps to reproduce. You should get an acknowledgement within a week.
