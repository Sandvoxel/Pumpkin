use std::{path::Path, process::Command};

fn main() {
    // Get short hash (7 chars) for display
    let short_output = Command::new("git")
        .args(["rev-parse", "--short=7", "HEAD"])
        .output();

    let git_hash_short = match short_output {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        _ => "unknown".to_string(),
    };

    // Get full hash for hover text
    let full_output = Command::new("git").args(["rev-parse", "HEAD"]).output();

    let git_hash_full = match full_output {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        _ => "unknown".to_string(),
    };

    println!("cargo::rerun-if-changed=build.rs");
    let git_paths = Command::new("git")
        .args([
            "rev-parse",
            "--git-path",
            "HEAD",
            "--git-path",
            "refs/heads",
            "--git-path",
            "packed-refs",
        ])
        .output();

    if let Ok(output) = git_paths
        && output.status.success()
    {
        for path in String::from_utf8_lossy(&output.stdout).lines() {
            if Path::new(path).exists() {
                println!("cargo::rerun-if-changed={path}");
            }
        }
    }
    println!("cargo::rustc-env=GIT_HASH={git_hash_short}");
    println!("cargo::rustc-env=GIT_HASH_FULL={git_hash_full}");
}
