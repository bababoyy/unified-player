use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/index");

    let revision =
        git_output(["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".to_owned());
    let dirty =
        git_output(["status", "--porcelain", "--untracked-files=no"]).map_or("unknown", |status| {
            if status.is_empty() {
                "false"
            } else {
                "true"
            }
        });
    println!("cargo:rustc-env=UNIFIED_PLAYER_GIT_REVISION={revision}");
    println!("cargo:rustc-env=UNIFIED_PLAYER_GIT_DIRTY={dirty}");
}

fn git_output<const N: usize>(arguments: [&str; N]) -> Option<String> {
    let output = Command::new("git").args(arguments).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
