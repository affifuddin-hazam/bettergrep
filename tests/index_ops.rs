//! Folder index files, organize, tracked moves and undo.

mod common;

use std::fs;
use std::time::{Duration, SystemTime};

use bettergrep::operations::{self, Actor};
use bettergrep::paths::INDEX_FILENAME;
use bettergrep::{folder_index, query};
use common::Env;
use serde_json::Value;

fn age(path: &str) {
    let f = fs::File::options().write(true).open(path).unwrap();
    f.set_modified(SystemTime::now() - Duration::from_secs(3600)).unwrap();
}

fn read_index(folder: &str) -> Value {
    serde_json::from_str(&fs::read_to_string(format!("{folder}\\{INDEX_FILENAME}")).unwrap()).unwrap()
}

#[test]
fn index_follows_a_file_through_two_moves() {
    let mut env = Env::new();
    for d in ["Downloads", "Docs", "Archive"] {
        fs::create_dir_all(env.p(d)).unwrap();
    }
    env.created("Downloads/cv.pdf", "resume");
    env.created("Downloads/tmp.txt", "x");
    fs::rename(env.p("Downloads/cv.pdf"), env.p("Docs/cv.pdf")).unwrap();
    env.tracker.moved(&env.p("Downloads/cv.pdf"), &env.p("Docs/cv.pdf"), false).unwrap();
    fs::rename(env.p("Docs/cv.pdf"), env.p("Archive/cv-2026.pdf")).unwrap();
    env.tracker.moved(&env.p("Docs/cv.pdf"), &env.p("Archive/cv-2026.pdf"), false).unwrap();
    fs::remove_file(env.p("Downloads/tmp.txt")).unwrap();
    env.tracker.deleted(&env.p("Downloads/tmp.txt")).unwrap();
    env.settle(10.0);

    let dl = folder_index::build(env.store(), &env.p("Downloads"), 500).unwrap();
    let departed = &dl["departed"][0];
    assert_eq!(departed["moved_to"], env.p("Docs/cv.pdf"));
    assert_eq!(departed["now_at"], env.p("Archive/cv-2026.pdf"), "points at the current location");
    assert_eq!(dl["deleted"][0]["name"], "tmp.txt");
    let arch = folder_index::build(env.store(), &env.p("Archive"), 500).unwrap();
    assert_eq!(arch["contents"][0]["came_from"], env.p("Docs/cv.pdf"));
    assert_eq!(arch["contents"][0]["how"], "moved");
    assert_eq!(dl.as_object().unwrap().keys().next().unwrap(), "schema", "schema stays first");
}

#[test]
fn index_file_is_atomic_hidden_and_not_rewritten_when_unchanged() {
    let mut env = Env::new();
    env.created("a.txt", "a");
    let folder = env.area.to_string_lossy().into_owned();
    let path = folder_index::write(env.store(), &folder, 500).unwrap().expect("written");
    assert!(bettergrep::win::is_hidden(&path));
    assert_eq!(read_index(&folder)["contents"][0]["name"], "a.txt");
    assert!(folder_index::write(env.store(), &folder, 500).unwrap().is_none());
    env.created("b.txt", "b");
    assert_eq!(folder_index::write(env.store(), &folder, 500).unwrap(), Some(path), "hidden index replaced");
}

#[test]
fn organize_dry_run_changes_nothing() {
    let env = Env::new();
    for name in ["photo.JPG", "invoice.pdf", "setup.exe", "weird.zzz"] {
        age(&env.write(name, "x"));
    }
    let folder = env.area.to_string_lossy().into_owned();
    let result = operations::organize(env.store(), &env.cfg, &folder, false, "cli", "test").unwrap();
    let cats: Vec<(String, String)> = result["planned"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["src"].as_str().unwrap().rsplit('\\').next().unwrap().to_string(),
                p["category"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        cats,
        [
            ("invoice.pdf".into(), "Documents".into()),
            ("photo.JPG".into(), "Images".into()),
            ("setup.exe".into(), "Installers".into())
        ]
    );
    assert_eq!(fs::read_dir(&env.area).unwrap().count(), 4);
}

#[test]
fn organize_apply_records_moves_writes_indexes_and_undo_restores() {
    let env = Env::new();
    age(&env.write("photo.jpg", "p"));
    age(&env.write("invoice.pdf", "i"));
    env.write("fresh.pdf", "still downloading");
    age(&env.write("Documents/invoice.pdf", "older invoice"));
    let folder = env.area.to_string_lossy().into_owned();

    let result = operations::organize(env.store(), &env.cfg, &folder, true, "cli", "bettergrep organize").unwrap();
    assert_eq!(result["moved"].as_array().unwrap().len(), 2, "{result:#}");
    assert!(env.area.join("Images/photo.jpg").exists());
    assert!(env.area.join("Documents/invoice (1).pdf").exists(), "name clash resolved");
    assert!(env.area.join("fresh.pdf").exists());

    let index = read_index(&folder);
    let photo = index["departed"].as_array().unwrap().iter().find(|d| d["name"] == "photo.jpg").unwrap();
    assert_eq!(photo["now_at"], env.p("Images/photo.jpg"));
    assert_eq!(photo["reason"], "category: Images");
    assert_eq!(read_index(&env.p("Images"))["contents"][0]["came_from"], env.p("photo.jpg"));
    let located = query::locate(env.store(), &env.p("photo.jpg")).unwrap();
    assert_eq!(located["matches"][0]["current_path"], env.p("Images/photo.jpg"), "former path resolves");

    let op = result["op_id"].as_str().unwrap();
    let undone = operations::undo(env.store(), &env.cfg, op, "cli", "bettergrep undo").unwrap();
    assert_eq!(undone["restored"].as_array().unwrap().len(), 2);
    assert_eq!(undone["removed_folders"].as_array().unwrap().len(), 1, "Images removed; Documents existed before");
    assert!(env.area.join("photo.jpg").exists() && !env.area.join("Images").exists());
    assert!(operations::undo(env.store(), &env.cfg, op, "cli", "x").is_err(), "cannot undo twice");
}

#[test]
fn tracked_move_refuses_overwrite_and_records_reason() {
    let env = Env::new();
    env.write("a.txt", "a");
    env.write("b.txt", "b");
    let who = || Actor { source: "cli", reason: Some("tidy"), ..Default::default() };
    assert!(operations::move_file(env.store(), &env.p("a.txt"), &env.p("b.txt"), who(), false).is_err());
    let out = operations::move_file(env.store(), &env.p("a.txt"), &env.p("sub/a.txt"), who(), false).unwrap();
    assert!(env.area.join("sub/a.txt").exists());
    let history = query::history(env.store(), out.file_id, 100).unwrap().unwrap();
    assert_eq!(history["events"].as_array().unwrap().last().unwrap()["reason"], "tidy");
}

#[test]
fn search_and_since_parsing() {
    let mut env = Env::new();
    env.created("Invoices/march-invoice.pdf", "m");
    env.created("photo.png", "p");
    let hits = query::search(env.store(), "invoice", Default::default()).unwrap();
    assert_eq!(hits.len(), 1);
    let globbed = query::search(env.store(), "*.png", Default::default()).unwrap();
    assert_eq!(globbed[0]["name"], "photo.png");
    let a = query::parse_since(Some("2h")).unwrap().unwrap();
    let b = query::parse_since(Some("120m")).unwrap().unwrap();
    assert!((a - b).abs() < 1.0);
    assert!(query::parse_since(Some("2026-01-31")).unwrap() < query::parse_since(Some("today")).unwrap());
    assert!(query::parse_since(Some("soon")).is_err());
}

#[test]
fn locate_by_former_name() {
    let mut env = Env::new();
    env.created("tax-2026.pdf", "taxes");
    fs::create_dir_all(env.p("Taxes")).unwrap();
    fs::rename(env.p("tax-2026.pdf"), env.p("Taxes/tax-2026-final.pdf")).unwrap();
    env.tracker.moved(&env.p("tax-2026.pdf"), &env.p("Taxes/tax-2026-final.pdf"), false).unwrap();
    let located = query::locate(env.store(), "tax-2026.pdf").unwrap();
    assert_eq!(located["matches"][0]["current_path"], env.p("Taxes/tax-2026-final.pdf"));
}
