use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("native")
        .join("x86_64-windows")
        .join("opentui.dll");
    println!("cargo:rerun-if-changed={}", source.display());
    if !source.is_file() {
        return;
    }
    let Ok(out) = env::var("OUT_DIR") else {
        return;
    };
    let out = PathBuf::from(out);
    let Some(profile) = out.ancestors().nth(4) else {
        return;
    };
    for directory in [profile.to_path_buf(), profile.join("deps")] {
        if fs::create_dir_all(&directory).is_ok() {
            let _ = fs::copy(&source, directory.join("opentui.dll"));
        }
    }
}
