fn main() {
    // Native Windows aliases must work without Developer Mode or symlink
    // privileges. Embed a tiny argv/stdio/exit-code forwarding launcher so the
    // existing signed rayline-<platform> asset can complete old installs too.
    println!("cargo:rerun-if-changed=src/legacy_alias.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR is set"))
            .join("rayline-alias.exe");
        let mut compiler =
            std::process::Command::new(std::env::var_os("RUSTC").expect("RUSTC is set"));
        compiler
            .args(["--edition=2021", "--crate-name=rayline_alias", "--target"])
            .arg(std::env::var("TARGET").expect("TARGET is set"))
            .args(["-C", "opt-level=s", "src/legacy_alias.rs", "-o"])
            .arg(output);
        if let Some(linker) = std::env::var_os("RUSTC_LINKER") {
            let mut flag = std::ffi::OsString::from("linker=");
            flag.push(linker);
            compiler.arg("-C").arg(flag);
        }
        assert!(
            compiler.status().expect("compile legacy alias").success(),
            "legacy alias build failed"
        );
    }
    println!("cargo:rerun-if-env-changed=RAYLINE_VERSION");
    println!("cargo:rerun-if-env-changed=RAYLINE_CHANNEL");

    let channel = std::env::var("RAYLINE_CHANNEL").unwrap_or_else(|_| "local".to_owned());
    println!("cargo:rustc-env=RAYLINE_CHANNEL={channel}");

    let version = std::env::var("RAYLINE_VERSION")
        .unwrap_or_else(|_| std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION is set"));
    println!("cargo:rustc-env=RAYLINE_VERSION={version}");
}
