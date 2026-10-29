use anyhow::{Context, Result, ensure};
use std::{path::Path, process::Command};

pub fn output(repo: &Path, args: &[&str]) -> Result<String> {
    Ok(raw_output(repo, args)?.trim().to_owned())
}

pub fn raw_output(repo: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(capture(repo, args, 1024 * 1024, None)?)?)
}

pub fn capture(
    repo: &Path,
    args: &[impl AsRef<std::ffi::OsStr>],
    limit: usize,
    active: Option<(&dmt::CancellationToken, std::time::Instant)>,
) -> Result<Vec<u8>> {
    use std::{io::Read, process::Stdio, sync::mpsc, time::Duration};
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("launch git")?;
    let mut stdout = child.stdout.take().context("missing git output")?;
    let (send, receive) = mpsc::sync_channel(2);
    let reader = std::thread::spawn(move || {
        loop {
            let mut chunk = vec![0; 16 * 1024];
            match stdout.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    chunk.truncate(n);
                    if send.send(Ok(chunk)).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = send.send(Err(error));
                    break;
                }
            }
        }
    });
    let result = (|| {
        let mut bytes = Vec::new();
        loop {
            if let Some((cancel, deadline)) = active {
                ensure!(!cancel.is_cancelled(), "review input capture cancelled");
                ensure!(
                    std::time::Instant::now() < deadline,
                    "review input capture deadline exceeded"
                );
            }
            match receive.recv_timeout(Duration::from_millis(20)) {
                Ok(chunk) => {
                    let chunk = chunk?;
                    ensure!(
                        bytes
                            .len()
                            .checked_add(chunk.len())
                            .is_some_and(|n| n <= limit),
                        "Git input exceeds byte limit"
                    );
                    bytes.extend(chunk);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => (),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        ensure!(
            child.wait()?.success(),
            "Git command failed while capturing review input"
        );
        Ok(bytes)
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    drop(receive);
    let _ = child.wait();
    let _ = reader.join();
    result
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
