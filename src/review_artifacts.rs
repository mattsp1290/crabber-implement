use crate::review_inputs::{RESPONSE_LIMIT, ReviewSnapshot};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use crabber::{
    ExtensionError, ToolDefinition,
    core::{ToolInfo, ToolOutput},
    extension::{Extension, Registrar, ToolContext, ToolExecutor},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub struct ReviewArtifacts {
    pub snapshot: ReviewSnapshot,
    pub standard_names: Vec<String>,
}
#[async_trait]
impl Extension for ReviewArtifacts {
    fn id(&self) -> &'static str {
        "crabber-implement/review-artifacts"
    }
    fn version(&self) -> &'static str {
        "1"
    }
    fn config_hash(&self) -> String {
        self.snapshot.identity().manifest.clone()
    }
    async fn install(&self, registrar: &mut Registrar) -> Result<(), ExtensionError> {
        let mut names = self.standard_names.clone();
        for catalog in [false, true] {
            let name = if catalog {
                "review_artifact_catalog"
            } else {
                "review_artifact_read"
            };
            names.push(name.into());
            let parameters = if catalog {
                json!({"type":"object","additionalProperties":false,"properties":{"cursor":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":32}},"required":["cursor","limit"]})
            } else {
                json!({"type":"object","additionalProperties":false,"properties":{"id":{"type":"string"},"offset":{"type":"integer","minimum":0},"length":{"type":"integer","minimum":1,"maximum":8192}},"required":["id","offset","length"]})
            };
            registrar.tool(Arc::new(ToolDefinition { info: ToolInfo { name: name.into(), description: if catalog { "List the immutable review snapshot inventory. Continue with next_cursor." } else { "Read pinned review evidence by artifact ID. Byte offsets; follow next_offset until truncated is false. Binary ranges use hex encoding." }.into(), parameters, retry_safe: true, required_permissions: vec![] }, executor: Arc::new(Reader { snapshot: self.snapshot.clone(), catalog }) }));
        }
        registrar.restrict_tools(names);
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadRequest {
    id: String,
    offset: usize,
    length: usize,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogRequest {
    cursor: usize,
    limit: usize,
}
struct Reader {
    snapshot: ReviewSnapshot,
    catalog: bool,
}

fn output(value: &Value) -> Result<Option<ToolOutput>> {
    let output = ToolOutput::from_json(value)?;
    Ok((serde_json::to_vec(&output)?.len() <= RESPONSE_LIMIT - 1024).then_some(output))
}
impl Reader {
    fn respond(&self, input: Value) -> Result<ToolOutput> {
        self.snapshot.verify_manifest()?;
        if self.catalog {
            let request: CatalogRequest = serde_json::from_value(input)?;
            let entries = &self.snapshot.manifest().entries;
            ensure!(
                request.limit > 0 && request.limit <= 32 && request.cursor <= entries.len(),
                "invalid catalog range"
            );
            let mut end = entries.len().min(request.cursor + request.limit);
            loop {
                let reply = json!({"entries":entries[request.cursor..end],"next_cursor":if end < entries.len() { Some(end) } else { None },"truncated":end < entries.len()});
                if let Some(output) = output(&reply)? {
                    return Ok(output);
                }
                ensure!(
                    end > request.cursor + 1,
                    "catalog entry exceeds response limit"
                );
                end -= 1;
            }
        }
        let request: ReadRequest = serde_json::from_value(input)?;
        ensure!(
            request.length > 0 && request.length <= 8192,
            "invalid read length"
        );
        let artifact = self
            .snapshot
            .manifest()
            .entries
            .iter()
            .find(|entry| entry.id == request.id)
            .context("unknown artifact ID")?;
        ensure!(request.offset <= artifact.bytes, "invalid read offset");
        let bytes = self
            .snapshot
            .range(artifact, request.offset, request.length)?;
        let mut end = bytes.len();
        loop {
            let range = &bytes[..end];
            let (encoding, content) = match std::str::from_utf8(range) {
                Ok(text) => ("utf8", text.to_owned()),
                Err(_) => ("hex", range.iter().map(|b| format!("{b:02x}")).collect()),
            };
            let reply = json!({"id":artifact.id,"offset":request.offset,"next_offset":request.offset + end,"total_bytes":artifact.bytes,"encoding":encoding,"content":content,"truncated":request.offset + end < artifact.bytes});
            if let Some(output) = output(&reply)? {
                return Ok(output);
            }
            ensure!(end > 1, "read metadata exceeds response limit");
            end /= 2;
        }
    }
}
#[async_trait]
impl ToolExecutor for Reader {
    async fn execute(&self, input: Value) -> Result<ToolOutput, ExtensionError> {
        self.respond(input).map_err(|_| {
            ExtensionError::Tool(
                "review artifact request failed validation or integrity check".into(),
            )
        })
    }
    async fn execute_with_context(
        &self,
        context: ToolContext,
        input: Value,
    ) -> Result<ToolOutput, ExtensionError> {
        if context.cancel.is_cancelled() {
            return Err(ExtensionError::Tool(
                "review artifact request cancelled".into(),
            ));
        }
        let result = self.execute(input).await;
        if context.cancel.is_cancelled() {
            return Err(ExtensionError::Tool(
                "review artifact request cancelled".into(),
            ));
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review_inputs::{PROMPT_LIMIT, SnapshotBuilder};

    fn fixture(bytes: &[u8]) -> (tempfile::TempDir, Reader) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().canonicalize().unwrap().join("snapshot");
        let mut builder = SnapshotBuilder::new(path, 1, "head".into()).unwrap();
        builder
            .add_origin(
                &uuid::Uuid::new_v4().to_string(),
                "evidence",
                "evidence.txt",
                "implement",
                0,
                bytes,
            )
            .unwrap();
        (
            root,
            Reader {
                snapshot: builder.finish().unwrap(),
                catalog: false,
            },
        )
    }
    #[test]
    fn encoded_output_is_bounded_and_losslessly_pageable() {
        for bytes in [
            vec![1; 32768],
            "\\\"\n🙂".repeat(8192).into_bytes(),
            vec![255; 32768],
        ] {
            let (_root, reader) = fixture(&bytes);
            let id = &reader.snapshot.manifest().entries[0].id;
            let mut offset = 0;
            let mut restored = Vec::new();
            loop {
                let result = reader
                    .respond(json!({"id":id,"offset":offset,"length":8192}))
                    .unwrap();
                assert!(serde_json::to_vec(&result).unwrap().len() <= RESPONSE_LIMIT);
                let result = result.json_value().unwrap();
                let content = result["content"].as_str().unwrap();
                if result["encoding"] == "hex" {
                    restored.extend(
                        (0..content.len())
                            .step_by(2)
                            .map(|i| u8::from_str_radix(&content[i..i + 2], 16).unwrap()),
                    );
                } else {
                    restored.extend_from_slice(content.as_bytes());
                }
                let next = result["next_offset"].as_u64().unwrap() as usize;
                assert!(next > offset);
                offset = next;
                if result["truncated"] == false {
                    break;
                }
            }
            assert_eq!(restored, bytes);
        }
    }
    #[test]
    fn reader_rejects_model_paths_ranges_and_sticky_corruption() {
        let (root, reader) = fixture(b"original");
        let id = reader.snapshot.manifest().entries[0].id.clone();
        for input in [
            json!({"id":"../secret","offset":0,"length":1}),
            json!({"id":id,"offset":0,"length":8193}),
            json!({"id":id,"offset":99,"length":1}),
            json!({"id":id,"offset":0,"length":1,"path":"/etc/passwd"}),
        ] {
            assert!(reader.respond(input).is_err());
        }
        let path = root.path().join("snapshot").join(&id);
        fs_write(&path, b"modified");
        assert!(
            reader
                .respond(json!({"id":id,"offset":0,"length":1}))
                .is_err()
        );
        fs_write(&path, b"original");
        assert!(reader.snapshot.verify().is_err());
    }
    fn fs_write(path: &std::path::Path, bytes: &[u8]) {
        std::fs::write(path, bytes).unwrap();
    }
    #[test]
    fn pinned_directory_prevents_ancestor_replacement_and_symlink_files() {
        let (root, reader) = fixture(b"original");
        let id = reader.snapshot.manifest().entries[0].id.clone();
        std::fs::rename(root.path().join("snapshot"), root.path().join("saved")).unwrap();
        let replacement = tempfile::tempdir().unwrap();
        std::fs::write(replacement.path().join(&id), b"external").unwrap();
        std::os::unix::fs::symlink(replacement.path(), root.path().join("snapshot")).unwrap();
        assert_eq!(
            reader
                .snapshot
                .bytes(&reader.snapshot.manifest().entries[0])
                .unwrap(),
            b"original"
        );
        assert!(
            ReviewSnapshot::open(&root.path().join("snapshot"), reader.snapshot.identity())
                .is_err()
        );
        let saved = root.path().join("saved").join(&id);
        std::fs::remove_file(&saved).unwrap();
        std::os::unix::fs::symlink(replacement.path().join(&id), &saved).unwrap();
        assert!(
            reader
                .respond(json!({"id":id,"offset":0,"length":1}))
                .is_err()
        );
    }
    #[test]
    fn catalog_and_rendering_bound_large_names_and_raw_excerpts() {
        let root = tempfile::tempdir().unwrap();
        let mut builder = SnapshotBuilder::new(
            root.path().canonicalize().unwrap().join("snapshot"),
            4,
            "head".into(),
        )
        .unwrap();
        for i in 0..200 {
            builder
                .add_origin(
                    &uuid::Uuid::new_v4().to_string(),
                    "evidence",
                    &format!("{i}{}", "\\\"".repeat(1000)),
                    "implement",
                    0,
                    b"unknown outcome",
                )
                .unwrap();
        }
        let snapshot = builder.finish().unwrap();
        for prompt in snapshot.prompts().unwrap() {
            assert!(prompt.len() <= PROMPT_LIMIT);
            assert!(prompt.contains("Additional inventory entries omitted"));
        }
        let reader = Reader {
            snapshot,
            catalog: true,
        };
        let mut cursor = 0;
        loop {
            let result = reader.respond(json!({"cursor":cursor,"limit":32})).unwrap();
            assert!(serde_json::to_vec(&result).unwrap().len() <= RESPONSE_LIMIT);
            let value = result.json_value().unwrap();
            if value["next_cursor"].is_null() {
                break;
            }
            cursor = value["next_cursor"].as_u64().unwrap();
        }
        assert!(cursor > 0);
    }
}
