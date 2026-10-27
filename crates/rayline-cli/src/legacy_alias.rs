//! Standalone Windows launcher, embedded in Raydock by build.rs.
//! Keep this dependency-free: old signed update assets also carry this alias.

fn main() {
    let result = std::env::current_exe().and_then(|path| {
        std::process::Command::new(path.with_file_name("raydock.exe"))
            .args(std::env::args_os().skip(1))
            .status()
    });
    match result {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(error) => {
            eprintln!("rayline: could not start raydock.exe: {error}. Reinstall Raydock.");
            std::process::exit(1);
        }
    }
}
