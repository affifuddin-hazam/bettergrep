//! End to end with real Windows notifications: the daemon, the registry watcher,
//! a traced "installer", and the MCP server over stdio.

mod common;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use bettergrep::config::Config;
use bettergrep::daemon::{self, Daemon};
use bettergrep::mcp::Server;
use bettergrep::paths::{INDEX_FILENAME, norm};
use bettergrep::registry::{self, Exclusions, RegistryWatcher};
use bettergrep::store::Store;
use bettergrep::trace::{self, RegistryMode, SessionOpts};
use bettergrep::{query, store};
use serde_json::{Value, json};

fn wait_for<T>(timeout: f64, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs_f64(timeout);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "condition not met in time");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn s(p: &Path) -> String {
    norm(p)
}

/// A unique HKCU test key, deleted on drop.
struct TestKey(String);

impl TestKey {
    fn new(prefix: &str) -> TestKey {
        let k = TestKey(format!(r"HKCU\Software\{prefix}-{}", &store::new_id()[..8]));
        k.reg(&["add", &k.0, "/f"]);
        k
    }

    fn reg(&self, args: &[&str]) {
        let ok = Command::new("reg").args(args).stdout(Stdio::null()).status().unwrap().success();
        assert!(ok, "reg {args:?} failed");
    }
}

impl Drop for TestKey {
    fn drop(&mut self) {
        let _ =
            Command::new("reg").args(["delete", &self.0, "/f"]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
}

#[test]
fn daemon_tracks_real_changes() {
    let dir = tempfile::tempdir().unwrap();
    let (inbox, shelf, outside) = (dir.path().join("Inbox"), dir.path().join("Shelf"), dir.path().join("Elsewhere"));
    for d in [&inbox, &shelf, &outside] {
        std::fs::create_dir_all(d).unwrap();
    }
    let mut cfg: Config = common::test_config(dir.path());
    cfg.settle_seconds = 0.3;
    cfg.move_window_seconds = 1.5;
    cfg.add_root(&s(&inbox), true, true);
    cfg.add_root(&s(&shelf), true, true); // separate watch: moves between them arrive as delete + create
    cfg.save().unwrap();

    let d = Daemon::new(cfg.clone(), true).unwrap();
    let halt = d.halt_flag();
    let handle = std::thread::spawn(move || d.run());
    wait_for(10.0, || daemon::read_state(&cfg)); // first heartbeat right after start-up
    let db = Store::open(&cfg.db_path()).unwrap();
    let present = |p: &Path| db.get_present(&s(p)).unwrap();

    std::fs::write(inbox.join("invoice.pdf"), "invoice v1").unwrap();
    let id = wait_for(10.0, || present(&inbox.join("invoice.pdf"))).id;

    std::fs::rename(inbox.join("invoice.pdf"), inbox.join("invoice-march.pdf")).unwrap();
    wait_for(10.0, || present(&inbox.join("invoice-march.pdf")));

    std::fs::rename(inbox.join("invoice-march.pdf"), shelf.join("invoice-march.pdf")).unwrap();
    let moved = wait_for(10.0, || present(&shelf.join("invoice-march.pdf")));
    assert_eq!(moved.id, id, "identity survives a move between watched trees");

    std::fs::copy(shelf.join("invoice-march.pdf"), inbox.join("copy.pdf")).unwrap();
    let copy = wait_for(10.0, || present(&inbox.join("copy.pdf")));
    assert_eq!(copy.copied_from, Some(id));

    // Moved to a folder nobody watches: followed by file ID instead of "deleted".
    std::fs::rename(inbox.join("copy.pdf"), outside.join("kept.pdf")).unwrap();
    let followed = wait_for(10.0, || db.get_file(copy.id).unwrap().filter(|r| r.path == s(&outside.join("kept.pdf"))));
    assert_eq!(followed.status, "present");

    // A folder appearing in one piece is adopted with its contents.
    std::fs::create_dir_all(outside.join("album")).unwrap();
    std::fs::write(outside.join("album").join("1.jpg"), "one").unwrap();
    std::fs::rename(outside.join("album"), inbox.join("album")).unwrap();
    wait_for(10.0, || present(&inbox.join("album").join("1.jpg")));

    let kinds: Vec<String> = query::history(&db, id, 100).unwrap().unwrap()["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(kinds, ["created", "renamed", "moved"]);

    let index = inbox.join(INDEX_FILENAME);
    let departed = wait_for(10.0, || {
        let data: Value = serde_json::from_str(&std::fs::read_to_string(&index).ok()?).ok()?;
        let list = data["departed"].as_array()?.clone();
        (list.len() >= 2).then_some(list)
    });
    let names: Vec<&str> = departed.iter().filter_map(|d| d["now_at"].as_str()).collect();
    assert!(names.contains(&s(&shelf.join("invoice-march.pdf")).as_str()), "{names:?}");
    assert!(names.contains(&s(&outside.join("kept.pdf")).as_str()), "{names:?}");

    halt.store(true, Ordering::Relaxed);
    handle.join().unwrap().unwrap();
    assert!(daemon::read_state(&cfg).is_none(), "state cleared on shutdown");
}

#[test]
fn second_daemon_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = common::test_config(dir.path());
    let _lock = daemon::InstanceLock::acquire(&cfg).expect("first lock");
    assert!(daemon::InstanceLock::acquire(&cfg).is_none());
    assert!(Daemon::new(cfg, false).unwrap().run().is_err());
}

#[test]
fn registry_snapshot_diff_and_notifications() {
    let key = TestKey::new("BettergrepTest");
    let none = Exclusions::default();
    let before = registry::snapshot(&key.0, None, &none).unwrap();
    key.reg(&["add", &format!(r"{}\Child", key.0), "/v", "InstallDir", "/d", r"C:\Apps\Demo", "/f"]);
    let after = registry::snapshot(&key.0, Some(&before), &none).unwrap();
    let mut kinds: Vec<&str> = registry::diff(&before, &after).iter().map(|c| c.kind).collect();
    kinds.sort();
    assert_eq!(kinds, ["reg_key_created", "reg_value_created"]);

    let (tx, rx) = crossbeam_channel::unbounded();
    let watcher = RegistryWatcher::start(
        vec![key.0.clone()],
        Duration::from_millis(200),
        Box::new(move |changes| {
            let _ = tx.send(changes);
        }),
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    key.reg(&["add", &key.0, "/v", "Autostart", "/d", "evil.exe", "/f"]);
    let changes = rx.recv_timeout(Duration::from_secs(10)).expect("no notification");
    watcher.stop();
    assert_eq!(changes.len(), 1);
    assert_eq!(
        (changes[0].kind, changes[0].value.as_deref(), changes[0].new.as_deref()),
        ("reg_value_created", Some("Autostart"), Some("evil.exe"))
    );
}

#[test]
fn trace_summarises_an_install() {
    let dir = tempfile::tempdir().unwrap();
    let (root, temp) = (dir.path().join("sys"), dir.path().join("temp"));
    std::fs::create_dir_all(root.join("Desktop")).unwrap();
    std::fs::create_dir_all(&temp).unwrap();
    std::fs::write(root.join("settings.ini"), "[x]\r\n").unwrap();
    std::fs::write(root.join("obsolete.dll"), "old").unwrap();
    let key = TestKey::new("BettergrepTrace");
    let script = dir.path().join("setup.cmd");
    let (r, t, k) = (s(&root), s(&temp), &key.0);
    std::fs::write(
        &script,
        format!(
            "@echo off\r\n\
             mkdir \"{t}\\setup-staging\"\r\n\
             for %%i in (1 2 3) do echo payload> \"{t}\\setup-staging\\payload%%i.cab\"\r\n\
             mkdir \"{r}\\Program Files\\DemoApp\\bin\"\r\n\
             echo MZ> \"{r}\\Program Files\\DemoApp\\bin\\demo.exe\"\r\n\
             echo hi> \"{r}\\Program Files\\DemoApp\\readme.txt\"\r\n\
             echo lnk> \"{r}\\Desktop\\DemoApp.lnk\"\r\n\
             echo demo=1>> \"{r}\\settings.ini\"\r\n\
             del \"{r}\\obsolete.dll\"\r\n\
             rmdir /s /q \"{t}\\setup-staging\"\r\n\
             reg add \"{k}\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\DemoApp\" /v DisplayName /d \"Demo App 1.0\" /f >nul\r\n\
             reg add \"{k}\\Microsoft\\Windows\\CurrentVersion\\Run\" /v DemoUpdater /d \"demo.exe --update\" /f >nul\r\n"
        ),
    )
    .unwrap();
    let mut cfg = common::test_config(dir.path());
    cfg.trace.registry_keys = vec![key.0.clone()];
    cfg.trace.baseline_seconds = 0.5;
    let db = Store::open(&cfg.db_path()).unwrap();
    let id = trace::run_session(
        &db,
        &cfg,
        SessionOpts {
            command: vec!["cmd".into(), "/c".into(), s(&script)],
            label: None,
            roots: vec![s(&root), s(&temp)],
            registry: RegistryMode::Full,
            settle_seconds: Some(1.0),
            keep_going: None,
            status: std::sync::Arc::new(|_| {}),
            attach: None,
        },
    )
    .unwrap();
    let session = query::session_json(&db.get_session(id).unwrap().unwrap());
    assert_eq!(session["status"], "finished");
    assert_eq!(session["exit_code"], 0);
    let sm = &session["summary"];
    assert_eq!(sm["new_folders"][0]["path"], s(&root.join("Program Files")));
    assert_eq!(sm["new_folders"][0]["files"], 2);
    assert_eq!(sm["loose_files_added"], json!([s(&root.join("Desktop").join("DemoApp.lnk"))]));
    assert_eq!(sm["modified_files"], json!([s(&root.join("settings.ini"))]));
    assert_eq!(sm["removed_files"], json!([s(&root.join("obsolete.dll"))]));
    assert_eq!(sm["extracted_then_removed"]["folders"][0]["path"], s(&temp.join("setup-staging")));
    assert_eq!(sm["registry"]["uninstall_entries"][0]["name"], "Demo App 1.0");
    let notable = sm["registry"]["notable"].as_array().unwrap();
    assert!(notable.iter().any(|n| n["category"] == "autostart" && n["value"] == "DemoUpdater"), "{notable:?}");
    let report = trace::render_markdown(&session);
    assert!(report.contains("DemoApp") && report.contains("Extracted, then cleaned up"));
}

#[test]
fn mcp_in_process_and_over_stdio() {
    let mut env = common::Env::new();
    let id = env.created("thesis.docx", "words");
    std::fs::create_dir_all(env.p("School")).unwrap();
    std::fs::rename(env.p("thesis.docx"), env.p("School/thesis.docx")).unwrap();
    env.tracker.moved(&env.p("thesis.docx"), &env.p("School/thesis.docx"), false).unwrap();

    let server = Server::new(env.cfg.clone(), false).unwrap();
    let rpc = |method: &str, params: Value| {
        server.handle(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})).unwrap()
    };
    assert_eq!(rpc("initialize", json!({"protocolVersion": "2025-03-26"}))["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(rpc("initialize", json!({"protocolVersion": "1999-01-01"}))["result"]["protocolVersion"], "2025-06-18");
    assert!(server.handle(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).is_none());
    assert_eq!(rpc("tools/list", json!({}))["result"]["tools"].as_array().unwrap().len(), 17);
    let located = rpc("tools/call", json!({"name": "locate_file", "arguments": {"target": "thesis.docx"}}));
    assert_eq!(located["result"]["isError"], false);
    assert_eq!(located["result"]["structuredContent"]["matches"][0]["current_path"], env.p("School/thesis.docx"));
    assert_eq!(located["result"]["structuredContent"]["matches"][0]["file"]["id"], id);
    assert_eq!(rpc("tools/call", json!({"name": "locate_file", "arguments": {"wrong": 1}}))["result"]["isError"], true);
    let no_drives = rpc("tools/call", json!({"name": "find_anywhere", "arguments": {"query": "thesis"}}));
    assert_eq!(no_drives["result"]["isError"], true, "full-drive mode is off in the test config");
    assert!(no_drives["result"]["content"][0]["text"].as_str().unwrap().contains("bettergrep drives add"));
    assert_eq!(rpc("tools/call", json!({"name": "nope"}))["error"]["code"], -32602);
    assert_eq!(rpc("no/such", json!({}))["error"]["code"], -32601);
    let read_only = Server::new(env.cfg.clone(), true).unwrap();
    assert!(!read_only.tool_names().contains(&"move_file"));

    let mut child = Command::new(env!("CARGO_BIN_EXE_bettergrep"))
        .args(["--home", &env.cfg.home, "mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let stdin = child.stdin.as_mut().unwrap();
        for msg in [
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}}),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "bettergrep_status", "arguments": {}}}),
        ] {
            writeln!(stdin, "{msg}").unwrap();
        }
    }
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    let replies: Vec<Value> =
        String::from_utf8(out.stdout).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(replies.iter().map(|r| r["id"].clone()).collect::<Vec<_>>(), [json!(1), json!(2)]);
    assert_eq!(replies[1]["result"]["structuredContent"]["data_dir"], env.cfg.home);
}
