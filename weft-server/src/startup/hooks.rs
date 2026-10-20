//! Startup phase 1 — the process-wide hooks: the tracing subscriber, and the OTLP
//! exporter when the operator configured a collector.
//!
//! Installed before anything else runs, so every later startup phase and every
//! request is observable. Installing a subscriber cannot fail the server: a
//! `try_init` is a no-op when one is already installed, and a collector that
//! cannot be reached falls back to the `fmt`-only path.

use weft_server::SERVICE;

/// Environment variable naming the OTLP collector endpoint. When set (e.g.
/// `http://localhost:4317`), `weft-server` exports its tracing spans to that collector
/// over OTLP/gRPC in addition to the `fmt` log subscriber (roadmap Phase 3). Unset
/// leaves tracing on the `fmt` path alone — no exporter, no network dependency.
const OTEL_ENDPOINT_ENV: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";

/// Install the process-wide tracing subscriber (roadmap Phase 3): a `fmt` layer
/// filtered by `RUST_LOG` (defaulting to `info`) that logs **span close** events, so
/// each span prints its recorded fields and its busy/idle duration on completion — the
/// "where did the time go" signal Phase 3 targets. All the request/compute/storage/
/// ingest/reconcile spans nest under the per-request `request` root.
///
/// When `OTEL_EXPORTER_OTLP_ENDPOINT` is set, an **OTLP/gRPC exporter** is attached
/// beside the `fmt` layer (the same shipped spans are exported to a collector like
/// Jaeger/Tempo/an OTel Collector) and the returned [`SdkTracerProvider`] is handed to
/// the caller to hold for the process lifetime and shut down on exit. When it is unset
/// there is no exporter and no network dependency — tracing stays on the `fmt` path.
///
/// `try_init` is a no-op when a subscriber is already installed, so this never panics.
pub(crate) fn init_tracing() -> Option<opentelemetry_sdk::trace::SdkTracerProvider> {
	use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _, Layer as _};

	let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
	let fmt_layer = tracing_subscriber::fmt::layer().with_target(false).with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE);

	let (otel_layer, provider) = match build_otlp_provider() {
		Some(provider) => {
			use opentelemetry::trace::TracerProvider as _;
			let tracer = provider.tracer(SERVICE);
			(Some(tracing_opentelemetry::layer().with_tracer(tracer).boxed()), Some(provider))
		}
		None => (None, None),
	};

	let _ = tracing_subscriber::registry().with(filter).with(fmt_layer).with(otel_layer).try_init();
	if provider.is_some() {
		println!("otlp trace export: enabled (exporting spans to {})", std::env::var(OTEL_ENDPOINT_ENV).unwrap_or_default());
	}
	provider
}

/// Build an OTLP tracer provider when `OTEL_EXPORTER_OTLP_ENDPOINT` is set (roadmap
/// Phase 3): an OTLP/gRPC `SpanExporter` at that endpoint, fed by a batch span
/// processor, tagged with the `weft-server` service resource. Returns `None` when the
/// env var is unset (export disabled) or the exporter cannot be built (logged, then
/// tracing falls back to the `fmt`-only path — a misconfigured collector must not stop
/// the server from starting).
fn build_otlp_provider() -> Option<opentelemetry_sdk::trace::SdkTracerProvider> {
	use opentelemetry_otlp::WithExportConfig as _;

	let endpoint = std::env::var(OTEL_ENDPOINT_ENV).ok()?;
	let exporter = match opentelemetry_otlp::SpanExporter::builder().with_tonic().with_endpoint(&endpoint).build() {
		Ok(exporter) => exporter,
		Err(err) => {
			eprintln!("otlp trace export: disabled — could not build the OTLP exporter for {endpoint:?}: {err}");
			return None;
		}
	};
	let resource = opentelemetry_sdk::Resource::builder().with_service_name(SERVICE).build();
	Some(opentelemetry_sdk::trace::SdkTracerProvider::builder().with_batch_exporter(exporter).with_resource(resource).build())
}
