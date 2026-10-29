use super::*;

#[test]
fn cli_admission_proof_limit_exports_safe_metadata_before_provider_dispatch() {
    let repo = repository();
    let fixture = Fixture::start(repo.path(), false);
    let intake = TraceIntake::start(TraceReply::Accepted);
    let plan = repo.path().join(".git/PRIVATE_ADMISSION_PATH_SENTINEL.md");
    let contents = format!(
        "PRIVATE_ADMISSION_INPUT_SENTINEL https://PRIVATE_ADMISSION_URL_SENTINEL/ {}",
        "\\".repeat(1_990_000)
    );
    assert!(contents.len() < 2_000_000);
    std::fs::write(&plan, contents).unwrap();
    let output = telemetry_command(repo.path(), &fixture, &plan, &intake.endpoint)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("admission") && error.contains("proof"),
        "{error}"
    );
    assert!(fixture.history.lock().unwrap().is_empty());
    let spans = intake.spans();
    let limited: Vec<_> = spans
        .iter()
        .filter(|span| trace_attribute(&span.attributes, "admission.limit_kind").is_some())
        .collect();
    assert!(!limited.is_empty(), "missing limit metadata in {spans:?}");
    for span in limited {
        assert_eq!(
            trace_attribute(&span.attributes, "admission.limit_kind").as_deref(),
            Some("proof_bytes")
        );
        assert_eq!(
            trace_attribute(&span.attributes, "admission.limit_bytes").as_deref(),
            Some("33554432")
        );
        assert!(span.attributes.iter().any(|attribute| {
            attribute.key == "admission.limit_bytes"
                && matches!(
                    attribute
                        .value
                        .as_ref()
                        .and_then(|value| value.value.as_ref()),
                    Some(any_value::Value::IntValue(33_554_432))
                )
        }));
        assert_eq!(
            trace_attribute(&span.attributes, "error.kind").as_deref(),
            Some("preparation_incomplete")
        );
        assert_eq!(
            trace_attribute(&span.attributes, "outcome").as_deref(),
            Some("unknown")
        );
    }
    let exported = format!("{spans:?}");
    for private in [
        "PRIVATE_ADMISSION_INPUT_SENTINEL",
        "PRIVATE_ADMISSION_PATH_SENTINEL",
        "PRIVATE_ADMISSION_URL_SENTINEL",
        "PRIVATE_TELEMETRY_KEY_SENTINEL",
        "PRIVATE_AGENTLESS_KEY_SENTINEL",
    ] {
        assert!(!exported.contains(private), "export leaked {private}");
    }
    export_artifacts(&run_artifacts(repo.path()), "admission-proof-limit");
}
