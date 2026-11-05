use crate::durable;
use anyhow::{Result, ensure};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub struct SessionArtifacts {
    pub transcript: fs::File,
    pub tools: fs::File,
    pub responses: fs::File,
    pub metadata: SessionMetadata,
}

impl SessionArtifacts {
    pub fn reserve(root: &Path, name: &str) -> Result<Self> {
        ensure!(
            !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "invalid session artifact name"
        );
        durable::directory(root)?;
        let mut created = Vec::new();
        let result = (|| {
            let mut files = Vec::new();
            for suffix in ["session.json", "txt", "tools.txt", "responses.jsonl"] {
                let path = root.join(format!("{name}.{suffix}"));
                files.push(durable::new_file(&path)?);
                created.push(path);
            }
            durable::sync_dir(root)?;
            let mut iter = files.into_iter();
            drop(iter.next());
            Ok(Self {
                metadata: SessionMetadata(created[0].clone()),
                transcript: iter.next().unwrap(),
                tools: iter.next().unwrap(),
                responses: iter.next().unwrap(),
            })
        })();
        if result.is_err() {
            for path in created {
                let _ = fs::remove_file(path);
            }
            let _ = durable::sync_dir(root);
        }
        result
    }
}

pub struct SessionMetadata(PathBuf);
impl SessionMetadata {
    pub fn persist(&self, value: &impl serde::Serialize) -> Result<()> {
        durable::write_json(&self.0, value, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn collision_preserves_every_existing_artifact() {
        for suffix in ["session.json", "txt", "tools.txt", "responses.jsonl"] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().canonicalize().unwrap();
            let file = path.join(format!("review-1-correctness.{suffix}"));
            fs::write(&file, b"original bytes").unwrap();
            assert!(SessionArtifacts::reserve(&path, "review-1-correctness").is_err());
            assert_eq!(fs::read(&file).unwrap(), b"original bytes");
            assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
        }
    }
}
