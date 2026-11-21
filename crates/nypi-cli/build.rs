//! Embeds the board's page (`assets/board/`, built by `scripts/build-board.sh` from
//! `crates/nypi-board-ui`) into the binary, so `nypi board` is one file. Without a built page the
//! table is empty and the board serves its API with a note on how to build the page.
//!
//! It also names the build (decision D98): `NYPI_TARGET`, the target triple, which picks a
//! release's archive for this machine, and `NYPI_GIT_SHA`, the commit, for `nypi --version`.

use std::path::Path;

fn main() {
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".into());
    println!("cargo:rustc-env=NYPI_TARGET={target}");
    let sha = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|sha| !sha.is_empty())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=NYPI_GIT_SHA={sha}");
    let head = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.git/HEAD");
    if head.exists() {
        println!("cargo:rerun-if-changed={}", head.display());
    }
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets").join("board");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_file())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| !n.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    let mut table = String::from("pub static ASSETS: &[(&str, &[u8])] = &[\n");
    for name in names {
        let path = dir.join(&name);
        println!("cargo:rerun-if-changed={}", path.display());
        table.push_str(&format!("    ({name:?}, include_bytes!({:?})),\n", path.display().to_string()));
    }
    table.push_str("];\n");
    let out = Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("board_assets.rs");
    std::fs::write(out, table).expect("write the board's asset table");
}
