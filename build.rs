//! Copy the sqlite3mc DLL and the Python indexer next to the built exe, so
//! `target/<profile>/` is a self-contained distribution folder.

use std::path::{Path, PathBuf};

// (in-repo path, dev-layout fallback in the parent directory)
const BUNDLE: [(&str, &str); 2] = [
    (
        "vendor/sqlite3mc_x64.dll",
        "UmaViewer-master/Assets/Plugins/sqlite3mc_x64.dll",
    ),
    ("uma_index_meta.py", "../uma_index_meta.py"),
];

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let root = manifest.parent().expect("repo root");
    // OUT_DIR = target/<profile>/build/<pkg>-<hash>/out
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let profile_dir = out.ancestors().nth(3).expect("target profile dir");
    for (repo, fallback) in BUNDLE {
        let src = [manifest.join(repo), root.join(fallback)]
            .into_iter()
            .find(|p| p.is_file());
        let Some(src) = src else {
            println!("cargo:warning=missing {repo}; not bundled");
            continue;
        };
        println!("cargo:rerun-if-changed={}", src.display());
        let name = Path::new(repo).file_name().unwrap();
        if let Err(e) = std::fs::copy(&src, profile_dir.join(name)) {
            println!("cargo:warning=could not bundle {}: {e}", src.display());
        }
    }
}
