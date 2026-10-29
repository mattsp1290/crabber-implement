use crabber::obs::{Signals, Telemetry, TelemetryConfig, TelemetryError};
use serde::Serialize;
use std::{env::VarError, path::Path, time::Duration};
use tracing::Span;
use tracing_subscriber::{filter::filter_fn, prelude::*};

use crate::agent::write_json_atomic;

pub struct RunTelemetry {
    exporter: Option<Telemetry>,
    report: Report,
}

#[derive(Serialize)]
struct Report {
    execution_generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_workflow_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recovery_mode: Option<&'static str>,
    transport: &'static str,
    status: &'static str,
    service: String,
    trace_id: Option<String>,
    error: Option<&'static str>,
    delivery: &'static str,
}

fn configuration(
    get: &dyn Fn(&str) -> Result<String, VarError>,
) -> Result<TelemetryConfig, TelemetryError> {
    let mut config = TelemetryConfig::from_lookup(&|name| {
        let optional = |key| get(key).ok().filter(|value| !value.is_empty());
        match name {
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT" => optional(name)
                .or_else(|| {
                    optional("OTEL_EXPORTER_OTLP_ENDPOINT")
                        .is_none()
                        .then(|| "http://127.0.0.1:4318/v1/traces".into())
                })
                .ok_or(VarError::NotPresent),
            "OTEL_EXPORTER_OTLP_TIMEOUT" => Ok(optional(name).unwrap_or_else(|| "2000".into())),
            "OTEL_EXPORTER_OTLP_ENDPOINT"
            | "OTEL_EXPORTER_OTLP_PROTOCOL"
            | "OTEL_EXPORTER_OTLP_HEADERS"
            | "DD_SERVICE"
            | "DD_ENV"
            | "DD_VERSION"
            | "OTEL_SERVICE_NAME" => optional(name).ok_or(VarError::NotPresent),
            _ => Err(VarError::NotPresent),
        }
    })?;
    if get("DD_SERVICE").ok().is_none_or(|value| value.is_empty())
        && get("OTEL_SERVICE_NAME")
            .ok()
            .is_none_or(|value| value.is_empty())
    {
        config.service = "crabber-implement".into();
    }
    config
        .version
        .get_or_insert_with(|| env!("CARGO_PKG_VERSION").into());
    if let Some(otlp) = &mut config.otlp {
        otlp.timeout = otlp
            .timeout
            .clamp(Duration::from_millis(1), Duration::from_secs(3));
    }
    config.signals = Signals {
        traces: true,
        metrics: false,
        logs: false,
    };
    Ok(config)
}

impl RunTelemetry {
    pub fn start(
        disabled: bool,
        workflow_run_id: &str,
        generation: u64,
        origin: Option<&crate::review_checkpoint::RecoveryOrigin>,
    ) -> Self {
        let mut report = Report {
            execution_generation: generation,
            parent_workflow_id: origin.map(|o| o.parent_workflow_id.clone()),
            recovery_mode: origin.map(|_| "refresh"),
            transport: "otlp_http_traces",
            status: "disabled",
            service: "crabber-implement".into(),
            trace_id: None,
            error: None,
            delivery: "unverified",
        };
        if disabled {
            return Self {
                exporter: None,
                report,
            };
        }
        let initialized = (|| {
            let mut config =
                configuration(&|name| std::env::var(name)).map_err(|_| "configuration")?;
            config
                .resource_tags
                .push(("workflow.run_id".into(), workflow_run_id.to_owned()));
            report.service = config.service.clone();
            let exporter = Telemetry::new(config).map_err(|_| "initialization")?;
            let installed = tracing_subscriber::registry()
                .with(exporter.layer().with_filter(filter_fn(|metadata| {
                    metadata.is_span()
                        && (metadata.name().starts_with("implement.")
                            || metadata.name().starts_with("crabber."))
                })))
                .try_init();
            Ok::<_, &'static str>((exporter, installed.is_ok()))
        })();
        match initialized {
            Ok((exporter, installed)) => {
                report.status = if installed { "enabled" } else { "failed" };
                if !installed {
                    report.error = Some("subscriber");
                    eprintln!("telemetry: subscriber initialization failed; workflow continues");
                }
                Self {
                    exporter: Some(exporter),
                    report,
                }
            }
            Err(category) => {
                report.status = "failed";
                report.error = Some(category);
                eprintln!("telemetry: {category} failed; workflow continues");
                Self {
                    exporter: None,
                    report,
                }
            }
        }
    }

    pub fn record_start(&mut self, artifacts: &Path, span: &Span) {
        self.report.trace_id = span.in_scope(|| {
            Telemetry::current_trace_context().map(|context| context.trace_id().to_owned())
        });
        self.persist(artifacts);
    }

    pub async fn finish(mut self, artifacts: &Path) {
        if let Some(exporter) = self.exporter {
            if exporter.shutdown().await.is_err() {
                self.report.status = "failed";
                self.report.error = Some("export");
                eprintln!(
                    "telemetry: trace export failed; workflow result is unchanged; see telemetry.json"
                );
            } else if self.report.error.is_none() {
                self.report.status = "flushed";
            }
        }
        if write_json_atomic(&artifacts.join("telemetry.json"), &self.report).is_err() {
            eprintln!("telemetry: could not save export health");
        }
    }

    fn persist(&self, artifacts: &Path) {
        if write_json_atomic(&artifacts.join("telemetry.json"), &self.report).is_err() {
            eprintln!("telemetry: could not save export health");
        }
    }
}

pub fn finish_span(span: &Span, success: bool, failure: &'static str) {
    span.record("outcome", if success { "completed" } else { "failed" });
    if !success {
        span.record("otel.status_code", "ERROR");
        span.record("error.kind", failure);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crabber::obs::OtlpProtocol;

    #[test]
    fn local_agent_defaults_never_load_agentless_credentials() {
        let config = configuration(&|name| {
            assert_ne!(name, "DD_API_KEY");
            Err(VarError::NotPresent)
        })
        .unwrap();
        assert!(config.datadog.is_none());
        assert_eq!(config.service, "crabber-implement");
        assert_eq!(
            config.otlp.as_ref().unwrap().traces_url,
            "http://127.0.0.1:4318/v1/traces"
        );
        assert!(!config.signals.metrics && !config.signals.logs);
    }

    #[test]
    fn protocol_and_timeout_apply_without_an_explicit_endpoint() {
        let config = configuration(&|name| match name {
            "OTEL_EXPORTER_OTLP_PROTOCOL" => Ok("http/json".into()),
            "OTEL_EXPORTER_OTLP_TIMEOUT" => Ok("87".into()),
            _ => Err(VarError::NotPresent),
        })
        .unwrap();
        let otlp = config.otlp.unwrap();
        assert_eq!(otlp.protocol, OtlpProtocol::HttpJson);
        assert_eq!(otlp.timeout, Duration::from_millis(87));
        for name in ["OTEL_EXPORTER_OTLP_PROTOCOL", "OTEL_EXPORTER_OTLP_TIMEOUT"] {
            assert!(
                configuration(&|key| if key == name {
                    Ok("invalid".into())
                } else {
                    Err(VarError::NotPresent)
                })
                .is_err()
            );
        }
    }

    #[test]
    fn explicit_endpoint_and_timeout_are_bounded() {
        let config = configuration(&|name| match name {
            "OTEL_EXPORTER_OTLP_ENDPOINT" => Ok("http://127.0.0.1:9999".into()),
            "OTEL_EXPORTER_OTLP_TIMEOUT" => Ok("999999".into()),
            _ => Err(VarError::NotPresent),
        })
        .unwrap();
        let otlp = config.otlp.unwrap();
        assert_eq!(otlp.traces_url, "http://127.0.0.1:9999/v1/traces");
        assert_eq!(otlp.timeout, Duration::from_secs(3));
    }
}
