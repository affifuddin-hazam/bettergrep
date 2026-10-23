//! Embed an application manifest: long-path aware (paths over 260 characters work
//! when Windows has `LongPathsEnabled`), UTF-8 code page, and `asInvoker` so the
//! tools never trigger a UAC prompt themselves.

use embed_manifest::manifest::{ActiveCodePage, Setting};
use embed_manifest::{embed_manifest, new_manifest};

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let manifest =
            new_manifest("bettergrep").long_path_aware(Setting::Enabled).active_code_page(ActiveCodePage::Utf8);
        embed_manifest(manifest).expect("unable to embed the application manifest");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
