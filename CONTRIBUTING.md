# Contributing

bettergrep watches people's files and registry, so correctness and predictability matter more than features.

## Setup and checks

```bash
cargo test                                  # unit, integration and live tests
cargo test -- --ignored recycle             # also sends a test file to YOUR Recycle Bin
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

The live tests (`tests/live.rs`) use real directory notifications, and they create and delete their own `HKCU\Software\BettergrepTest-*` and `BettergrepTrace-*` registry keys. Tests must never write to `HKLM` or to real autostart locations.

If your checkout path is long, set `CARGO_TARGET_DIR` to a short directory; the MSVC linker cannot write paths over 260 characters.

## Ground rules

- **Formats are versioned.** A database schema change means bumping `SCHEMA_VERSION` and adding a migration. New fields in `config.json` and `.bettergrep.json` must be optional, so older versions can still read them, and must be documented in `docs/`.
- **The database is the source of truth.** Index files, CLI output and MCP results are projections of it.
- **Never lose identity silently.** A change to `tracker.rs` needs a test in `tests/tracker.rs` that replays the exact notification sequence Windows produces.
- **bettergrep never deletes user files.** Operations move files and can be undone. Undo removes only folders that the undone operation created, and only when they are empty.
- **`unsafe` stays in `win.rs`, `watcher.rs`, `registry.rs` and the launch code in `trace.rs`.** Every `unsafe` block needs a `SAFETY:` comment saying why it is sound (alignment, buffer bounds, handle lifetime).
- Document user-visible changes in `CHANGELOG.md`.
