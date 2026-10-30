use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use crabber::auth::{AuthError, CredentialProfile, CredentialStore, OAuthCredentials};
use serde::Deserialize;
use std::path::{Path, PathBuf};

const AUTH_LIMIT: usize = 64 * 1024;

#[derive(Clone)]
pub struct CodexAuthFileStore {
    path: PathBuf,
}

#[derive(Deserialize)]
struct NativeAuth {
    auth_mode: Option<String>,
    tokens: Option<NativeTokens>,
}

#[derive(Deserialize)]
struct NativeTokens {
    access_token: String,
    account_id: String,
}

#[derive(Deserialize)]
struct Expiry {
    exp: u64,
}

pub fn default_path() -> Result<PathBuf> {
    let directory = match std::env::var_os("CODEX_HOME") {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".codex"),
    };
    Ok(directory.join("auth.json"))
}

pub fn validate(path: &Path) -> Result<()> {
    let credentials = CodexAuthFileStore::new(path.to_owned())
        .load()
        .map_err(|_| anyhow::anyhow!("Codex ChatGPT login is unavailable or invalid; run `codex login` and use its auth.json"))?
        .context("Codex ChatGPT login is missing; run `codex login`")?;
    ensure!(
        credentials.expires_unix_ms > crabber::auth::now_ms().saturating_add(60_000),
        "Codex access token has expired or expires shortly; refresh your sign-in with Codex before running"
    );
    Ok(())
}

impl CodexAuthFileStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl CredentialStore for CodexAuthFileStore {
    fn load(&self) -> std::result::Result<Option<OAuthCredentials>, AuthError> {
        match std::fs::symlink_metadata(&self.path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(AuthError::Store),
            Ok(_) => (),
        }
        let bytes = crate::durable::read(&self.path, AUTH_LIMIT).map_err(|_| AuthError::Store)?;
        let auth: NativeAuth = serde_json::from_slice(&bytes).map_err(|_| AuthError::Store)?;
        if auth.auth_mode.as_deref() != Some("chatgpt") {
            return Err(AuthError::ProfileMismatch);
        }
        let tokens = auth.tokens.ok_or(AuthError::NoCredentials)?;
        if tokens.account_id.trim().is_empty() || tokens.access_token.trim().is_empty() {
            return Err(AuthError::NoCredentials);
        }
        let mut parts = tokens.access_token.split('.');
        let header = parts.next().ok_or(AuthError::Store)?;
        let payload = parts.next().ok_or(AuthError::Store)?;
        let signature = parts.next().ok_or(AuthError::Store)?;
        if header.is_empty() || payload.is_empty() || signature.is_empty() || parts.next().is_some()
        {
            return Err(AuthError::Store);
        }
        let payload = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| AuthError::Store)?;
        let claims: Expiry = serde_json::from_slice(&payload).map_err(|_| AuthError::Store)?;
        let expires_unix_ms = claims
            .exp
            .checked_mul(1000)
            .filter(|expiry| *expiry > 0)
            .ok_or(AuthError::Store)?;
        Ok(Some(OAuthCredentials {
            profile: CredentialProfile::CodexCli,
            client_id: crabber::auth::codex::CODEX_CLI_CLIENT_ID.into(),
            host_id: "urn:crabber-implement:codex-cli".into(),
            account_id: tokens.account_id,
            access_token: tokens.access_token,
            refresh_token: None,
            id_token: String::new(),
            scopes: Vec::new(),
            expires_unix_ms,
        }))
    }

    fn save(&self, _: &OAuthCredentials) -> std::result::Result<(), AuthError> {
        Err(AuthError::Store)
    }

    fn clear(&self) -> std::result::Result<(), AuthError> {
        Err(AuthError::Store)
    }

    fn compare_exchange(
        &self,
        _: &OAuthCredentials,
        _: Option<&OAuthCredentials>,
    ) -> std::result::Result<bool, AuthError> {
        Err(AuthError::Store)
    }

    fn host_id(&self) -> std::result::Result<String, AuthError> {
        Ok("urn:crabber-implement:codex-cli".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    fn auth(path: &Path, expires: u64, access: &str) -> Vec<u8> {
        let payload = URL_SAFE_NO_PAD.encode(json!({"exp":expires}).to_string());
        let value = json!({"auth_mode":"chatgpt","tokens":{"account_id":"fixture-account","access_token":format!("header.{payload}.{access}"),"refresh_token":"PRIVATE_REFRESH_SENTINEL","id_token":"PRIVATE_ID_SENTINEL"}});
        let bytes = serde_json::to_vec(&value).unwrap();
        std::fs::write(path, &bytes).unwrap();
        bytes
    }

    #[test]
    fn native_credentials_are_access_only_and_cannot_change_the_codex_store() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("auth.json");
        let bytes = auth(&path, u64::MAX / 1000, "signature");
        let store = CodexAuthFileStore::new(path.clone());
        let credentials = store.load().unwrap().unwrap();
        assert_eq!(credentials.profile, CredentialProfile::CodexCli);
        assert_eq!(credentials.account_id, "fixture-account");
        assert!(credentials.refresh_token.is_none());
        assert!(credentials.id_token.is_empty());
        assert!(!format!("{credentials:?}").contains("signature"));
        assert!(store.save(&credentials).is_err());
        assert!(store.clear().is_err());
        assert!(store.compare_exchange(&credentials, None).is_err());
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }

    #[tokio::test]
    async fn token_manager_reads_rotation_without_ever_refreshing_or_writing() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("auth.json");
        auth(&path, u64::MAX / 1000, "first");
        let manager =
            crabber::auth::TokenManager::new(Arc::new(CodexAuthFileStore::new(path.clone())))
                .with_profile(CredentialProfile::CodexCli);
        let first = manager.credentials().await.unwrap();
        assert!(manager.force_refresh(&first.access_token).await.is_err());
        let rotated = auth(&path, u64::MAX / 1000, "second");
        let second = manager.force_refresh(&first.access_token).await.unwrap();
        assert_ne!(first.access_token, second.access_token);
        assert_eq!(std::fs::read(&path).unwrap(), rotated);
        let expired = auth(&path, 1, "expired");
        assert!(manager.credentials().await.is_err());
        assert!(validate(&path).unwrap_err().to_string().contains("expired"));
        assert_eq!(std::fs::read(path).unwrap(), expired);
    }

    #[test]
    fn malformed_api_key_oversized_symlink_and_special_files_fail_safely() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("auth.json");
        let store = CodexAuthFileStore::new(path.clone());
        assert!(store.load().unwrap().is_none());
        for contents in [
            json!({"auth_mode":"apikey","OPENAI_API_KEY":"PRIVATE_KEY_SENTINEL"}),
            json!({"auth_mode":"chatgpt","tokens":{"account_id":"","access_token":"header.payload.signature"}}),
            json!({"auth_mode":"chatgpt","tokens":{"account_id":"account","access_token":"PRIVATE_TOKEN_SENTINEL"}}),
        ] {
            std::fs::write(&path, contents.to_string()).unwrap();
            let error = validate(&path).unwrap_err().to_string();
            assert!(!error.contains("PRIVATE_"));
            assert!(error.contains("Codex"));
        }
        std::fs::File::create(&path)
            .unwrap()
            .set_len(AUTH_LIMIT as u64 + 1)
            .unwrap();
        assert!(store.load().is_err());
        std::fs::remove_file(&path).unwrap();
        let target = root.path().join("target.json");
        auth(&target, u64::MAX / 1000, "signature");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(store.load().is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(store.load().is_err());
        std::fs::remove_dir(&path).unwrap();
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        assert!(store.load().is_err());
    }
}
