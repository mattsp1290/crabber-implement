use crabber::{
    AdmissionLimitKind, Agent, AgentConfig, FakeProvider, PermissionDecision, Selection,
    StaticPolicy, StreamDelta,
};
use std::{sync::Arc, time::Duration};

fn agent(provider: &FakeProvider) -> Agent {
    Agent::builder()
        .behavior_version("bounded-review-admission.v1")
        .store(Arc::new(crabber::session::MemoryStore::new()))
        .provider(Arc::new(provider.clone()))
        .config(AgentConfig::new(Selection {
            provider_id: "fake".into(),
            model_id: "scripted".into(),
        }))
        .policy(Arc::new(StaticPolicy::new(PermissionDecision::Allow)))
        .build()
        .unwrap()
}

#[tokio::test]
async fn reviewer_ceiling_fits_actual_memory_admission_with_escaped_and_multibyte_inputs() {
    for atom in ["\\", "\"", "\n", "🦀"] {
        let prompt = atom.repeat((256 * 1024) / atom.len());
        assert_eq!(prompt.len(), 256 * 1024);
        let provider = FakeProvider::scripted(vec![vec![
            StreamDelta::TextDelta("done".into()),
            StreamDelta::Completed,
        ]]);
        let agent = agent(&provider);
        let result = agent
            .prompt(None, prompt)
            .await
            .unwrap()
            .done()
            .await
            .unwrap();
        assert_eq!(result.status, crabber::core::RunStatus::Completed);
        assert_eq!(provider.requests().len(), 1);
        assert!(agent.close(Duration::from_secs(2)).await.local_drained());
    }
}

#[tokio::test]
async fn proof_limit_preserves_its_type_and_uncertain_preparation_without_dispatch() {
    let provider = FakeProvider::default();
    let agent = agent(&provider);
    let Err(error) = agent.prompt(None, "\\".repeat(1_990_000)).await else {
        panic!("oversized escaped request was admitted");
    };
    let limit = error.admission_limit().expect("typed proof limit");
    assert_eq!(limit.kind(), AdmissionLimitKind::ProofBytes);
    assert_eq!(limit.limit_bytes(), Some(33_554_432));
    let reconciliation = error.preparation_reconciliation().unwrap().clone();
    assert!(provider.requests().is_empty());
    let close = agent.close(Duration::from_secs(2)).await;
    assert!(
        close
            .runtime
            .uncertain_preparations
            .contains(&reconciliation)
    );
}

#[tokio::test]
async fn ordinary_invalid_input_does_not_become_a_proof_limit() {
    let provider = FakeProvider::default();
    let agent = agent(&provider);
    let Err(error) = agent.prompt(None, "\u{1}".repeat(1_900_000)).await else {
        panic!("invalid typed input was admitted");
    };
    assert!(error.admission_limit().is_none());
    assert!(provider.requests().is_empty());
    let _ = agent.close(Duration::from_secs(2)).await;
}
