use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    // localreel-core/crates/localreel-api -> ../../../localreel-web/dist
    let dist = manifest.join("../../../localreel-web/dist");
    println!("cargo:rerun-if-changed={}", dist.display());
    println!("cargo:rerun-if-changed=build.rs");

    let mut files = Vec::new();
    if dist.is_dir() {
        collect(&dist, &dist, &mut files);
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));

    let mut code = String::from("pub static FILES: &[(&str, &[u8])] = &[\n");
    for (relative, absolute) in files {
        let path = absolute.to_string_lossy().replace('\\', "/");
        code.push_str(&format!(
            "    ({relative:?}, include_bytes!(r#\"{path}\"#)),\n"
        ));
    }
    code.push_str("];\n");

    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("web_assets.rs");
    fs::write(&out, code).expect("failed to write embedded web assets");
}

fn collect(root: &Path, current: &Path, files: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = fs::read_dir(current) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, files);
        } else if path.is_file() {
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            files.push((relative, path));
        }
    }
}
