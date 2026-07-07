use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    emit_rerun_hints();

    let version = env_override()
        .or_else(exact_tag_version)
        .or_else(commit_date_version)
        .unwrap_or_else(|| "canary-unknown".to_string());

    println!("cargo:rustc-env=RTPBRIDGE_BUILD_VERSION={version}");
}

fn env_override() -> Option<String> {
    ["BUILD_VERSION", "RTPBRIDGE_BUILD_VERSION"]
        .into_iter()
        .find_map(|key| {
            env::var(key)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
}

fn exact_tag_version() -> Option<String> {
    let tag = git_output(&["describe", "--tags", "--exact-match", "HEAD"])?;
    let tag = tag.strip_prefix('v').unwrap_or(&tag);
    Some(format!("v{tag}"))
}

fn commit_date_version() -> Option<String> {
    let date = git_output(&[
        "show",
        "-s",
        "--format=%cd",
        "--date=format-local:%y.%-m%d.%-H%M",
        "HEAD",
    ])?;
    Some(format!("canary-{date}"))
}

fn git_output(args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .env("TZ", "UTC0")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn emit_rerun_hints() {
    println!("cargo:rerun-if-env-changed=BUILD_VERSION");
    println!("cargo:rerun-if-env-changed=RTPBRIDGE_BUILD_VERSION");

    let Some(git_dir) = git_dir() else {
        return;
    };

    println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());
    println!(
        "cargo:rerun-if-changed={}",
        git_dir.join("packed-refs").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        git_dir.join("refs/tags").display()
    );

    if let Ok(head) = fs::read_to_string(git_dir.join("HEAD"))
        && let Some(head_ref) = head.trim().strip_prefix("ref: ")
    {
        println!(
            "cargo:rerun-if-changed={}",
            git_dir.join(head_ref).display()
        );
    }
}

fn git_dir() -> Option<PathBuf> {
    let dot_git = Path::new(".git");
    if dot_git.is_dir() {
        return Some(dot_git.to_path_buf());
    }

    let git_file = fs::read_to_string(dot_git).ok()?;
    let git_dir = git_file.trim().strip_prefix("gitdir: ")?;
    Some(PathBuf::from(git_dir))
}
