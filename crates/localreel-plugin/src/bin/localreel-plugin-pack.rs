//! Build a `.lrplug` package from a plugin directory.
//!
//! ```text
//! localreel-plugin-pack --dir path/to/plugin --out dist/webdav.lrplug [--force]
//! localreel-plugin-pack ... [--signing-key publisher.seed]
//! ```

use std::path::PathBuf;

use localreel_plugin::package;

fn usage(message: &str) -> ! {
    eprintln!("{message}");
    eprintln!(
        "usage: localreel-plugin-pack --dir <plugin-dir> --out <package.lrplug> [--force] [--signing-key <32-byte-hex-file>]"
    );
    std::process::exit(2);
}

fn main() {
    let mut dir: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut signing_key: Option<PathBuf> = None;
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
            "--signing-key" => {
                signing_key = Some(
                    args.next()
                        .map(PathBuf::from)
                        .unwrap_or_else(|| usage("--signing-key needs a file")),
                )
            }
            "--force" => force = true,
            "--help" | "-h" => {
                println!(
                    "usage: localreel-plugin-pack --dir <plugin-dir> --out <package.lrplug> [--force] [--signing-key <32-byte-hex-file>]"
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
            if let Some(key) = signing_key {
                match std::fs::read_to_string(&key)
                    .map_err(|error| error.to_string())
                    .and_then(|seed| package::sign_package(&out, &seed).map_err(|e| e.to_string()))
                {
                    Ok((public_key, signature)) => {
                        println!("public_key: {public_key}");
                        println!("signature: {signature}");
                    }
                    Err(error) => {
                        eprintln!("sign failed: {error}");
                        std::process::exit(1);
                    }
                }
            }
        }
        Err(error) => {
            eprintln!("pack failed: {error}");
            std::process::exit(1);
        }
    }
}
