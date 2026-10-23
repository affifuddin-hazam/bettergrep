//! Lineage engine: every way a file can appear, move, copy or disappear.

mod common;

use std::fs;

use common::Env;

#[test]
fn new_file_is_recorded_after_it_settles() {
    let mut env = Env::new();
    let path = env.write("report.pdf", "hello");
    env.tracker.created(&path, false).unwrap();
    env.tracker.tick().unwrap();
    assert!(env.store().get_present(&path).unwrap().is_none(), "recorded before the quiet period");
    env.settle(2.0);
    let row = env.store().get_present(&path).unwrap().unwrap();
    assert!(row.sha256.is_some());
    assert_eq!(row.size, Some(5));
    assert_eq!(env.kinds(None), ["created"]);
}

#[test]
fn rename_keeps_identity() {
    let mut env = Env::new();
    let id = env.created("a.txt", "a");
    fs::rename(env.p("a.txt"), env.p("b.txt")).unwrap();
    env.tracker.moved(&env.p("a.txt"), &env.p("b.txt"), false).unwrap();
    assert_eq!(env.store().get_present(&env.p("b.txt")).unwrap().unwrap().id, id);
    assert_eq!(env.kinds(Some(id)), ["created", "renamed"]);
}

#[test]
fn delete_then_create_with_same_file_id_is_a_move() {
    let mut env = Env::new();
    let id = env.created("in/photo.jpg", "pixels");
    fs::create_dir_all(env.p("out")).unwrap();
    fs::rename(env.p("in/photo.jpg"), env.p("out/photo.jpg")).unwrap();
    env.tracker.deleted(&env.p("in/photo.jpg")).unwrap();
    env.tracker.created(&env.p("out/photo.jpg"), false).unwrap();
    env.settle(2.0);
    env.settle(10.0);
    assert_eq!(env.store().get_present(&env.p("out/photo.jpg")).unwrap().unwrap().id, id);
    assert_eq!(env.kinds(Some(id)), ["created", "moved"]);
}

#[test]
fn copy_then_delete_is_a_move() {
    let mut env = Env::new();
    let id = env.created("a/big.iso", &"x".repeat(1000));
    fs::copy(env.p("a/big.iso"), env.p("b.iso")).unwrap();
    env.tracker.created(&env.p("b.iso"), false).unwrap();
    env.settle(2.0);
    assert!(env.kinds(None).contains(&"copied".to_string()));
    fs::remove_file(env.p("a/big.iso")).unwrap();
    env.tracker.deleted(&env.p("a/big.iso")).unwrap();
    env.settle(10.0);
    assert_eq!(env.store().get_present(&env.p("b.iso")).unwrap().unwrap().id, id);
    assert_eq!(env.kinds(Some(id)), ["created", "moved"]);
    assert_eq!(env.store().scalar_i64("SELECT COUNT(*) FROM files", vec![]).unwrap(), 1);
}

#[test]
fn delete_is_held_back_then_committed() {
    let mut env = Env::new();
    let id = env.created("old.log", "log");
    fs::remove_file(env.p("old.log")).unwrap();
    env.tracker.deleted(&env.p("old.log")).unwrap();
    env.settle(1.0);
    assert_eq!(env.store().get_file(id).unwrap().unwrap().status, "present");
    env.settle(10.0);
    assert_eq!(env.store().get_file(id).unwrap().unwrap().status, "deleted");
    assert_eq!(env.kinds(Some(id)), ["created", "deleted"]);
}

#[test]
fn file_moved_out_of_tracked_folders_is_followed_by_id() {
    let mut env = Env::new();
    let id = env.created("tracked/contract.pdf", "signed");
    let elsewhere = env.dir.path().join("somewhere-else").join("deep");
    fs::create_dir_all(&elsewhere).unwrap();
    let dest = elsewhere.join("contract-final.pdf");
    fs::rename(env.p("tracked/contract.pdf"), &dest).unwrap();
    // Only the source folder is watched: all we ever see is a delete.
    env.tracker.deleted(&env.p("tracked/contract.pdf")).unwrap();
    env.settle(10.0);
    let row = env.store().get_file(id).unwrap().unwrap();
    assert_eq!(row.status, "present");
    assert_eq!(row.path, bettergrep::paths::norm(&dest));
    assert_eq!(env.kinds(Some(id)), ["created", "moved"]);
}

#[test]
fn copy_is_linked_to_its_origin() {
    let mut env = Env::new();
    let original = env.created("song.mp3", "music");
    fs::copy(env.p("song.mp3"), env.p("backup.mp3")).unwrap();
    env.tracker.created(&env.p("backup.mp3"), false).unwrap();
    env.settle(2.0);
    let copy = env.store().get_present(&env.p("backup.mp3")).unwrap().unwrap();
    assert_eq!(copy.copied_from, Some(original));
    assert_eq!(env.kinds(Some(copy.id)), ["copied"]);
}

#[test]
fn replace_in_place_keeps_identity() {
    let mut env = Env::new();
    let id = env.created("doc.docx", "v1");
    fs::remove_file(env.p("doc.docx")).unwrap();
    env.tracker.deleted(&env.p("doc.docx")).unwrap();
    env.write("doc.docx", "version two");
    env.tracker.created(&env.p("doc.docx"), false).unwrap();
    env.settle(2.0);
    env.settle(10.0);
    let row = env.store().get_present(&env.p("doc.docx")).unwrap().unwrap();
    assert_eq!(row.id, id);
    assert_eq!(row.size, Some("version two".len() as i64));
    assert_eq!(env.kinds(Some(id)), ["created", "modified"]);
}

#[test]
fn partial_download_rename() {
    let mut env = Env::new();
    let partial = env.write("setup.exe.crdownload", "binary");
    env.tracker.created(&partial, false).unwrap();
    fs::rename(&partial, env.p("setup.exe")).unwrap();
    env.tracker.moved(&partial, &env.p("setup.exe"), false).unwrap();
    env.settle(2.0);
    assert!(env.store().get_present(&env.p("setup.exe")).unwrap().is_some());
    assert_eq!(env.kinds(None), ["created"]);
}

#[test]
fn folder_move_carries_children() {
    let mut env = Env::new();
    fs::create_dir_all(env.p("proj")).unwrap();
    env.tracker.created(&env.p("proj"), true).unwrap();
    let a = env.created("proj/a.txt", "a");
    let b = env.created("proj/sub/b.txt", "b");
    fs::create_dir_all(env.p("archive")).unwrap();
    fs::rename(env.p("proj"), env.p("archive/proj")).unwrap();
    env.tracker.moved(&env.p("proj"), &env.p("archive/proj"), true).unwrap();
    assert_eq!(env.store().get_present(&env.p("archive/proj/a.txt")).unwrap().unwrap().id, a);
    assert_eq!(env.store().get_present(&env.p("archive/proj/sub/b.txt")).unwrap().unwrap().id, b);
    env.tracker.moved(&env.p("proj/a.txt"), &env.p("archive/proj/a.txt"), false).unwrap();
    env.settle(2.0);
    assert_eq!(env.kinds(Some(a)), ["created", "moved"]);
}

#[test]
fn folder_arriving_with_contents_is_adopted() {
    let mut env = Env::new();
    // A folder appears in one go (moved in from an unwatched place): only one event.
    env.write("incoming/photos/1.jpg", "one");
    env.write("incoming/photos/2.jpg", "two");
    env.tracker.created(&env.p("incoming"), true).unwrap();
    env.settle(2.0);
    for rel in ["incoming", "incoming/photos", "incoming/photos/1.jpg", "incoming/photos/2.jpg"] {
        assert!(env.store().get_present(&env.p(rel)).unwrap().is_some(), "{rel} not adopted");
    }
}

#[test]
fn short_lived_file_is_ignored() {
    let mut env = Env::new();
    let path = env.write("scratch.bin", "x");
    env.tracker.created(&path, false).unwrap();
    fs::remove_file(&path).unwrap();
    env.tracker.deleted(&path).unwrap();
    env.settle(10.0);
    assert!(env.kinds(None).is_empty());
}

#[test]
fn modification_only_when_content_changes() {
    let mut env = Env::new();
    let id = env.created("notes.txt", "one");
    env.write("notes.txt", "two!");
    env.tracker.modified(&env.p("notes.txt"), false).unwrap();
    env.settle(2.0);
    assert_eq!(env.kinds(Some(id)), ["created", "modified"]);
}

#[test]
fn reconcile_detects_offline_changes() {
    let mut env = Env::new();
    env.write("keep.txt", "keep");
    env.write("gone.txt", "gone");
    env.write("wanderer.txt", "wander");
    let root = env.area.to_string_lossy().into_owned();
    let stats = env.tracker.reconcile(&root, true).unwrap();
    assert_eq!(stats.discovered, 3);
    let wanderer = env.store().get_present(&env.p("wanderer.txt")).unwrap().unwrap().id;

    fs::remove_file(env.p("gone.txt")).unwrap();
    fs::create_dir_all(env.p("sub")).unwrap();
    fs::rename(env.p("wanderer.txt"), env.p("sub/wanderer.txt")).unwrap();
    env.advance(60.0);
    let stats = env.tracker.reconcile(&root, true).unwrap();
    assert_eq!((stats.missing, stats.moved), (1, 1), "{stats:?}");
    assert_eq!(env.store().get_present(&env.p("sub/wanderer.txt")).unwrap().unwrap().id, wanderer);
    assert_eq!(env.store().get_latest_at(&env.p("gone.txt")).unwrap().unwrap().status, "missing");
}

#[test]
fn reconcile_follows_files_moved_away_while_offline() {
    let mut env = Env::new();
    env.write("tracked/thesis.docx", "words");
    let root = env.p("tracked");
    env.tracker.reconcile(&root, true).unwrap();
    let id = env.store().get_present(&env.p("tracked/thesis.docx")).unwrap().unwrap().id;
    let dest = env.dir.path().join("Documents").join("thesis.docx");
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    fs::rename(env.p("tracked/thesis.docx"), &dest).unwrap();
    let stats = env.tracker.reconcile(&root, true).unwrap();
    assert_eq!((stats.followed, stats.missing), (1, 0));
    assert_eq!(env.store().get_file(id).unwrap().unwrap().path, bettergrep::paths::norm(&dest));
}

#[test]
fn ignored_files_never_recorded() {
    let mut env = Env::new();
    for rel in ["~$draft.docx", "thumbs.db", ".bettergrep.json", "node_modules/pkg/index.js"] {
        let path = env.write(rel, "x");
        env.tracker.created(&path, false).unwrap();
    }
    env.settle(2.0);
    assert!(env.kinds(None).is_empty());
}

/// Sends a real file to the Recycle Bin, so it is not run by default:
/// `cargo test -- --ignored recycle`.
#[test]
#[ignore]
fn recycled_file_is_reported_as_recycled() {
    use windows_sys::Win32::UI::Shell::{FO_DELETE, FOF_ALLOWUNDO, FOF_NO_UI, SHFILEOPSTRUCTW, SHFileOperationW};

    let mut env = Env::new();
    let id = env.created("recycle-me.txt", "bye");
    let mut from: Vec<u16> = env.p("recycle-me.txt").encode_utf16().collect();
    from.extend([0, 0]);
    let mut op: SHFILEOPSTRUCTW = unsafe { std::mem::zeroed() };
    op.wFunc = FO_DELETE;
    op.pFrom = from.as_ptr();
    op.fFlags = (FOF_ALLOWUNDO | FOF_NO_UI) as u16;
    assert_eq!(unsafe { SHFileOperationW(&mut op) }, 0);
    env.tracker.deleted(&env.p("recycle-me.txt")).unwrap();
    env.settle(10.0);
    let details: String = env
        .store()
        .conn
        .query_row("SELECT details FROM events WHERE file_id = ? AND kind = 'deleted'", [id], |r| r.get(0))
        .unwrap();
    assert!(details.contains("\"recycled\":true"), "{details}");
}
