use async_trait::async_trait;
use crabber::{
    ReasoningEffort, Selection,
    core::ProviderBindingDescriptor,
    providers::{
        DeltaStream, HttpAdapter, HttpResolver, ModelRequest, ProviderBindingError, ProviderError,
        Resolver, Streamer,
    },
};
use std::sync::Arc;

pub struct CodexProvider {
    resolver: HttpResolver,
    effort: ReasoningEffort,
}
impl CodexProvider {
    pub fn new(config: &crate::config::RunConfig) -> Self {
        Self {
            resolver: HttpResolver::new().with_adapter(
                HttpAdapter::codex_cli(Arc::new(crate::codex_auth::CodexAuthFileStore::new(
                    config.codex_auth_file.clone(),
                )))
                .with_base_url(&config.base_url),
            ),
            effort: config.reasoning_effort,
        }
    }
}

#[async_trait]
impl Resolver for CodexProvider {
    fn binding_descriptor(
        &self,
        selection: &Selection,
    ) -> Result<ProviderBindingDescriptor, ProviderBindingError> {
        let descriptor = self.resolver.binding_descriptor(selection)?;
        let configuration =
            serde_json::to_vec(&(&descriptor, self.effort)).expect("provider identity serializes");
        ProviderBindingDescriptor::new(
            descriptor.provider_id(),
            descriptor.model_id(),
            descriptor.backend_id(),
            "crabber-implement.codex.v3",
            crabber::InputFingerprint::new(crate::durable::digest(&configuration))
                .expect("digest is valid"),
            descriptor.limits(),
        )
        .map_err(|_| ProviderBindingError::InvalidDescriptor)
    }

    async fn resolve(&self, selection: &Selection) -> Result<Arc<dyn Streamer>, ProviderError> {
        Ok(Arc::new(CodexStream {
            inner: self.resolver.resolve(selection).await?,
            effort: self.effort,
        }))
    }
}

struct CodexStream {
    inner: Arc<dyn Streamer>,
    effort: ReasoningEffort,
}
#[async_trait]
impl Streamer for CodexStream {
    async fn stream(&self, mut request: ModelRequest) -> Result<DeltaStream, ProviderError> {
        request.reasoning_effort = Some(self.effort);
        self.inner.stream(request).await
    }
}
