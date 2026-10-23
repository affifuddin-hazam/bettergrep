# Trace sessions

A trace answers one question: what did this program change on my system? It covers files installed, modified or removed, files extracted and cleaned up again, and registry keys and values written.

```bash
bettergrep trace --label "VLC 3.0.21" --report vlc.md -- vlc-3.0.21-win64.exe
bettergrep trace                         # manual: do the install yourself, press Enter when done
bettergrep session start --label "Driver update"   # background; stop with: bettergrep session stop
bettergrep session show 4                # report for a past session
```

## What happens

1. **Watch.** Start recursive watchers on the trace roots. The default is the system drive. Change it with `--root` (repeatable) or `trace.roots` in the config.
2. **Registry baseline.** Snapshot the trace registry scope:
   - `full` (default): `HKCU\Software`, `HKLM\SOFTWARE` (including `WOW6432Node` and `Classes`) and `HKLM\SYSTEM\CurrentControlSet\Services`, minus a few huge subtrees that Windows itself churns (`trace.registry_exclude`). The subtrees are read in parallel, about 8 s for ~500,000 keys on a typical machine.
   - `quick`: only the background-monitor keys (autostart, services, uninstall, environment, …). Under a second.
   - `off`.
3. **Learn background activity.** Everything that changes before the program starts (during the registry baseline, and for at least `trace.baseline_seconds`) marks its folder as *busy*. File changes in busy folders are reported separately as background activity, so a browser cache or another app's database doesn't show up as "installed". Folders are never treated as background, so a new install folder inside a busy parent is still reported.
4. **Launch** the program. If Windows reports that it needs elevation (error 740), bettergrep asks through the normal UAC prompt.
5. **Record** until the program exits, then for `--settle` seconds more (default 5), because installers often hand off to `msiexec` or a service.
6. **Diff the registry** against the baseline (in parallel, reusing unchanged keys), then **summarize**. If Windows dropped notifications during the session, the report says so.

## Reading the summary

| Section | Meaning |
|---|---|
| New folders | Top-level folders created during the session, with file counts and sizes. Usually the install directory. |
| Files added elsewhere | New files outside those folders: shortcuts, DLLs dropped into system folders, config files. |
| Modified / removed files | Pre-existing files changed or deleted. |
| Extracted, then cleaned up | Files created *and* deleted during the session: installer payloads unpacked to `%TEMP%` and removed. |
| Registry counts and new keys | The topmost created keys (children are implied). |
| Uninstall entries | `DisplayName` values created under `…\Uninstall\…`. |
| Notable changes | Autostart (`Run`/`RunOnce`/`Winlogon`), services, scheduled tasks, IFEO debugger hijacks, environment, shell extensions, file associations, COM classes. |
| Background activity | File changes in folders that were already busy before launch. |

All raw events are kept. `bettergrep session show <id> --events` or the MCP tool `trace_session_report` with `include_events: true` lists them.

## Limits, honestly

- **Attribution is by time, not by process.** Anything else that writes during the session is included. Background learning removes most of it, but a program that first wakes up mid-trace will still appear. Close busy apps for the cleanest results.
- **Packaged-app virtualization.** Programs running inside an MSIX container (including some terminals and apps launched from Store apps) have writes to `%LOCALAPPDATA%` and `HKCU` redirected under `%LOCALAPPDATA%\Packages\<app>\LocalCache`. The trace shows the real, redirected location.
- **Notification overflow.** `ReadDirectoryChangesExW` can drop events if a huge number of files change at once. The recorder detects this and adds a warning to the report. The summary re-checks existence on disk at the end, but a file created and deleted inside a dropped burst can be missed.
- **Registry scope.** Changes outside the trace scope (for example `HKLM\SYSTEM` beyond services, or `HKU` of other users) aren't captured, and neither are changes inside excluded subtrees. Adjust `trace.registry_keys` and `trace.registry_exclude` as needed.
- **Values are bounded.** String values are stored up to 512 characters and binary values up to 64 bytes, each with a hash of the full data, so changes are still detected.

Per-process attribution through ETW (Event Tracing for Windows) is a possible future backend; it requires administrator rights.
