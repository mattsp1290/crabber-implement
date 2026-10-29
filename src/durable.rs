use anyhow::{Context, Result, ensure};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
};

pub const METADATA_LIMIT: usize = 1024 * 1024;
pub const REPORT_LIMIT: usize = 8 * 1024 * 1024;
pub const PROMPT_LIMIT: usize = 16 * 1024 * 1024;

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn sync_dir(path: &Path) -> Result<()> {
    ensure!(
        fs::symlink_metadata(path)?.is_dir()
            && !fs::symlink_metadata(path)?.file_type().is_symlink(),
        "unsafe artifact directory"
    );
    fs::File::open(path)?
        .sync_all()
        .context("sync artifact directory")?;
    fault("directory_sync")
}

pub fn directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => directory_existing(path)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().context("artifact directory has no parent")?;
            directory(parent)?;
            fs::DirBuilder::new().mode(0o700).create(path)?;
            if let Err(error) = fault("directory_create").and_then(|()| sync_dir(parent)) {
                fs::remove_dir(path).context("remove unconfirmed artifact directory")?;
                fs::File::open(parent)?.sync_all()?;
                return Err(error);
            }
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

pub fn directory_existing(path: &Path) -> Result<()> {
    for ancestor in path.ancestors().filter(|p| !p.as_os_str().is_empty()) {
        let stat = fs::symlink_metadata(ancestor)?;
        ensure!(
            stat.is_dir() && !stat.file_type().is_symlink(),
            "unsafe checkpoint directory"
        );
    }
    Ok(())
}

pub fn read(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("read {}", path.display()))?;
    ensure!(file.metadata()?.is_file(), "artifact is not a regular file");
    ensure!(
        file.metadata()?.len() <= limit as u64,
        "artifact exceeds size limit"
    );
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "artifact exceeds size limit");
    Ok(bytes)
}

pub fn json<T: DeserializeOwned>(path: &Path, limit: usize) -> Result<T> {
    serde_json::from_slice(&read(path, limit)?).context("invalid checkpoint JSON")
}

pub fn new_file(path: &Path) -> Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("reserve {}", path.display()))
}

pub fn bytes(path: &Path, data: &[u8], replace: bool) -> Result<()> {
    let parent = path.parent().context("artifact has no parent")?;
    directory(parent)?;
    if replace && fs::symlink_metadata(path).is_ok() {
        ensure!(
            fs::symlink_metadata(path)?.is_file()
                && !fs::symlink_metadata(path)?.file_type().is_symlink(),
            "unsafe artifact destination"
        );
    }
    let temporary = parent.join(format!(".write-{}", uuid::Uuid::new_v4()));
    let mut published = false;
    let result = (|| {
        let mut file = new_file(&temporary)?;
        file.write_all(data)?;
        fault("file_write")?;
        file.sync_all()?;
        fault("file_sync")?;
        if replace {
            fs::rename(&temporary, path)?;
        } else {
            fs::hard_link(&temporary, path)?;
            published = true;
            fs::remove_file(&temporary)?;
        }
        fault("publish")?;
        sync_dir(parent)
    })();
    let _ = fs::remove_file(&temporary);
    if result.is_err() && published {
        fs::remove_file(path).context("remove unconfirmed artifact publication")?;
        fs::File::open(parent)?
            .sync_all()
            .context("sync failed publication removal")?;
    }
    #[cfg(test)]
    if result.is_ok()
        && path
            .file_name()
            .is_some_and(|name| name == "00-overview.md")
        && path
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == "correctness")
        && let Some(marker) = std::env::var_os("CRABBER_TEST_RENDER_PAUSE")
    {
        fs::write(marker, b"first rendered file durably published")?;
        loop {
            std::thread::park();
        }
    }
    result
}

pub fn write_json(path: &Path, value: &impl Serialize, replace: bool) -> Result<()> {
    bytes(path, &serde_json::to_vec_pretty(value)?, replace)
}

#[cfg(not(test))]
fn fault(_: &str) -> Result<()> {
    Ok(())
}

#[cfg(test)]
thread_local! { pub static FAIL: std::cell::RefCell<Option<&'static str>> = const { std::cell::RefCell::new(None) }; }
#[cfg(test)]
fn fault(step: &str) -> Result<()> {
    ensure!(
        !FAIL.with(|f| *f.borrow() == Some(step)),
        "injected durable {step} failure"
    );
    Ok(())
}

pub(crate) fn barrier(_point: &str) -> Result<()> {
    #[cfg(test)]
    if std::env::var("CRABBER_TEST_REFRESH_BARRIER")
        .ok()
        .as_deref()
        == Some(_point)
        && let Some(marker) = std::env::var_os("CRABBER_TEST_REFRESH_MARKER")
    {
        fs::write(marker, _point.as_bytes())?;
        loop {
            std::thread::park();
        }
    }
    Ok(())
}
