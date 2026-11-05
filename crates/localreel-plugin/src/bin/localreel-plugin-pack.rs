//! Build a `.lrplug` package from a plugin directory.
//!
//! ```text
//! localreel-plugin-pack --dir path/to/plugin --out dist/webdav.lrplug [--force]
//! ```

use std::path::PathBuf;

use localreel_plugin::package;

fn usage(message: &str) -> ! {
    eprintln!("{message}");
    eprintln!("usage: localreel-plugin-pack --dir <plugin-dir> --out <package.lrplug> [--force]");
    std::process::exit(2);
}

fn main() {
    let mut dir: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut force = false;
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--dir" => {
                dir = Some(
                    args.next()
                        .map(PathBuf::from)
                        .unwrap_or_else(|| usage("--dir needs a directory")),
                )
            }
            "--out" => {
                out = Some(
                    args.next()
                        .map(PathBuf::from)
                        .unwrap_or_else(|| usage("--out needs a path")),
                )
            }
            "--force" => force = true,
            "--help" | "-h" => {
                println!(
                    "usage: localreel-plugin-pack --dir <plugin-dir> --out <package.lrplug> [--force]"
                );
                return;
            }
            other => usage(&format!("unknown argument {other:?}")),
        }
    }
    let dir = dir.unwrap_or_else(|| usage("--dir is required"));
    let out = out.unwrap_or_else(|| usage("--out is required"));
    if out.exists() && !force {
        eprintln!(
            "{} already exists; pass --force to replace it",
            out.display()
        );
        std::process::exit(1);
    }
    match package::pack(&dir, &out) {
        Ok(report) => {
            println!(
                "packed {} {} -> {} ({} files, {} bytes)",
                report.manifest.id,
                report.manifest.version,
                report.path.display(),
                report.files,
                report.bytes
            );
            println!("sha256: {}", report.sha256);
        }
        Err(error) => {
            eprintln!("pack failed: {error}");
            std::process::exit(1);
        }
    }
}
