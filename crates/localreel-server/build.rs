//! Embed the Windows icon and version information into the executable.
//!
//! Without these resources Explorer shows a generic blank icon, the Start Menu
//! shortcut has nothing to point at, and Task Manager/file properties show no
//! product name — which is what "looks like a half-finished program" means in
//! practice. The resource compiler ships with the MSVC build tools, which the
//! `x86_64-pc-windows-msvc` target already requires.

fn main() {
    println!("cargo:rerun-if-changed=assets/localreel.ico");

    #[cfg(windows)]
    {
        let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
        let numeric = format!("{version}.0");

        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/localreel.ico");
        resource.set("CompanyName", "LocalReel");
        resource.set("FileDescription", "LocalReel - local-first media server");
        resource.set("FileVersion", &numeric);
        resource.set("InternalName", "localreel");
        resource.set("LegalCopyright", "Copyright 2026 LocalReel contributors");
        resource.set("OriginalFilename", "localreel.exe");
        resource.set("ProductName", "LocalReel");
        resource.set("ProductVersion", &numeric);
        resource
            .compile()
            .expect("failed to compile Windows resources");
    }
}
