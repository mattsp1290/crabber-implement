use anyhow::{Context, Result, ensure};
use std::{path::Path, process::Command};

pub fn output(repo: &Path, args: &[&str]) -> Result<String> {
    Ok(raw_output(repo, args)?.trim().to_owned())
}

pub fn raw_output(repo: &Path, args: &[&str]) -> Result<String> {
    let result = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .context("launch git")?;
    ensure!(
        result.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&result.stderr).trim()
    );
    Ok(String::from_utf8(result.stdout)?)
}

pub fn require_clean(repo: &Path) -> Result<()> {
    ensure!(
        output(repo, &["status", "--porcelain"])?.is_empty(),
        "repository has uncommitted changes; commit or stash them before running"
    );
    Ok(())
}

pub fn commit_changes(repo: &Path, message: &str) -> Result<()> {
    output(repo, &["add", "--all", "--", "."])?;
    if !output(repo, &["diff", "--cached", "--name-only"])?.is_empty() {
        output(
            repo,
            &[
                "-c",
                "user.name=Crabber Implement",
                "-c",
                "user.email=crabber-implement@localhost",
                "commit",
                "-m",
                message,
            ],
        )?;
    }
    Ok(())
}
