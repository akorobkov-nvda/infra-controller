/*
 * SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
 * SPDX-License-Identifier: Apache-2.0
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 * http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! Builds the OTLP span-export layer for service binaries.
//!
//! Exports spans only when a collector endpoint is configured. A rejected
//! endpoint logs a warning and leaves the service running.
//!
//! The returned layer carries its own span filter, so the span level changes
//! without affecting log output. Attach the log `EnvFilter` to each log layer,
//! not to the registry, because a registry filter also applies to this layer.
//!
//! ```no_run
//! use tracing_subscriber::layer::SubscriberExt;
//! use tracing_subscriber::util::SubscriberInitExt;
//! use tracing_subscriber::Layer;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let log_filter = tracing_subscriber::EnvFilter::builder()
//!     .with_default_directive(tracing_subscriber::filter::LevelFilter::INFO.into())
//!     .from_env_lossy();
//!
//! let (span_layer, tracing) = carbide_instrument::otlp_tracing::setup(
//!     carbide_instrument::otlp_tracing::Config::new("nico-pxe"),
//! );
//!
//! tracing_subscriber::registry()
//!     .with(span_layer)
//!     .with(logfmt::layer().with_filter(log_filter))
//!     .try_init()?;
//!
//! tracing.report();
//! // ... run the service ...
//! # Ok(()) }
//! ```
//!
//! # Shutdown
//!
//! The exporter sends spans in batches on a timer. Call [`Tracing::shutdown`]
//! before the process exits to send the current batch.
//!
//! # Sampling
//!
//! This module installs no sampler, so `OTEL_TRACES_SAMPLER` and
//! `OTEL_TRACES_SAMPLER_ARG` take effect. Prefer `parentbased_traceidratio`,
//! which applies the ratio only where a trace starts. Note that a sampler drops
//! spans before the collector sees them, so leave it at the default when the
//! collector selects traces by latency or errors.

use opentelemetry::trace::TracerProvider as _;
use opentelemetry::{KeyValue, global};
use opentelemetry_otlp::WithExportConfig as _;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::registry::LookupSpan;

/// Standard OTLP endpoint variables, in precedence order. The trace-only variable
/// comes first, then the one that also applies to metrics and logs.
const ENDPOINT_VARS: [&str; 2] = [
    opentelemetry_otlp::OTEL_EXPORTER_OTLP_TRACES_ENDPOINT,
    opentelemetry_otlp::OTEL_EXPORTER_OTLP_ENDPOINT,
];

/// Sets the most verbose span level exported, separately from the log level. A
/// span's level comes from the macro that creates it, such as `info_span!`.
/// OpenTelemetry defines no equivalent variable, so this module reads it
/// directly.
pub const SPAN_LEVEL_VAR: &str = "NICO_TRACES_SPAN_LEVEL";

/// How a service configures span export.
#[derive(Debug, Clone)]
pub struct Config {
    /// Sets `service.name` on the exported spans and names the tracer. Use the
    /// deployed component name, such as `nico-pxe` rather than `carbide-pxe`.
    pub service_name: &'static str,
    /// Collector endpoint taken from the service's own config file or CLI
    /// flags. The standard OTLP variables override this value.
    pub config_endpoint: Option<String>,
    /// Most verbose span level exported when [`SPAN_LEVEL_VAR`] is unset.
    pub default_span_level: LevelFilter,
}

impl Config {
    /// Creates a config that exports spans only when an OTLP variable supplies
    /// an endpoint. Exports at `INFO`.
    pub fn new(service_name: &'static str) -> Self {
        Self {
            service_name,
            config_endpoint: None,
            default_span_level: LevelFilter::INFO,
        }
    }

    /// Sets the fallback endpoint used when no OTLP variable is set.
    #[must_use]
    pub fn with_config_endpoint(mut self, endpoint: Option<String>) -> Self {
        self.config_endpoint = endpoint;
        self
    }

    /// Sets the span level used when [`SPAN_LEVEL_VAR`] is unset. Pass `DEBUG` or
    /// `TRACE` to export more spans, for example behind a `--debug` flag.
    #[must_use]
    pub fn with_default_span_level(mut self, level: LevelFilter) -> Self {
        self.default_span_level = level;
        self
    }
}

/// The span-export layer to pass to `SubscriberExt::with`.
///
/// `None` means no endpoint is configured. `tracing_subscriber` accepts `None`
/// as a layer that does nothing, so the caller needs no branch.
pub type SpanLayer<S> = Option<Box<dyn Layer<S> + Send + Sync>>;

/// Holds the tracer provider for as long as the process runs.
pub struct Tracing {
    provider: Option<SdkTracerProvider>,
    state: State,
    span_level: LevelFilter,
    invalid_span_level: Option<String>,
}

enum State {
    Off,
    On {
        endpoint: String,
    },
    Failed {
        endpoint: String,
        error: opentelemetry_otlp::ExporterBuildError,
    },
}

impl Tracing {
    /// Logs how span export was configured. Call once, after initializing the
    /// subscriber, otherwise the messages are discarded.
    pub fn report(&self) {
        match &self.state {
            State::Off => {
                tracing::debug!(
                    traces_var = ENDPOINT_VARS[0],
                    generic_var = ENDPOINT_VARS[1],
                    "no OTLP endpoint configured; span export off"
                );
            }
            State::On { endpoint } => {
                tracing::info!(
                    endpoint = %endpoint,
                    span_level = %self.span_level,
                    "exporting spans over OTLP/gRPC"
                );
            }
            State::Failed { endpoint, error } => {
                // The service does not need a working collector. A rejected
                // endpoint logs a warning and the process keeps running.
                tracing::warn!(
                    endpoint = %endpoint,
                    %error,
                    "OTLP span exporter could not be built; continuing without span export"
                );
            }
        }

        if let Some(value) = &self.invalid_span_level {
            tracing::warn!(
                var = SPAN_LEVEL_VAR,
                %value,
                fallback = %self.span_level,
                "ignoring unparseable span level"
            );
        }
    }

    /// Sends the current batch of spans, then shuts the exporter down. Does
    /// nothing when span export is off. Runs on `spawn_blocking` because
    /// `SdkTracerProvider::shutdown` blocks for up to five seconds.
    pub async fn shutdown(self) {
        let Some(provider) = self.provider else {
            return;
        };

        match tokio::task::spawn_blocking(move || provider.shutdown()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(%error, "failed to flush OpenTelemetry spans on shutdown");
            }
            Err(error) => {
                tracing::warn!(%error, "OpenTelemetry shutdown task failed");
            }
        }
    }
}

/// Builds the span-export layer and installs the W3C trace-context propagator.
///
/// Call before initializing the subscriber, and [`Tracing::report`] after. Never
/// fails: with no endpoint, or one the exporter rejects, the layer is `None` and
/// `report` logs the reason.
///
/// The propagator is required for `traceparent` headers. OpenTelemetry's default
/// does nothing, so without this every service starts its own trace.
pub fn setup<S>(config: Config) -> (SpanLayer<S>, Tracing)
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a> + Send + Sync,
{
    global::set_text_map_propagator(TraceContextPropagator::new());

    let (span_level, invalid_span_level) =
        span_level(|var| std::env::var(var).ok(), config.default_span_level);
    let endpoint = endpoint(
        |var| std::env::var(var).ok(),
        config.config_endpoint.as_deref(),
    );

    let (provider, state) = match endpoint {
        None => (None, State::Off),
        Some(endpoint) => match build_span_exporter(&endpoint) {
            Ok(exporter) => (
                Some(build_tracer_provider(exporter, config.service_name)),
                State::On { endpoint },
            ),
            Err(error) => (None, State::Failed { endpoint, error }),
        },
    };

    let layer = provider.as_ref().map(|provider| {
        let filter = tracing_subscriber::filter::filter_fn(move |metadata| {
            exportable(span_level, metadata.level(), metadata.module_path())
        })
        // Reports the lowest level this layer accepts. Without the hint, a span
        // filter set to `TRACE` enables every `trace!` call in the process and
        // formats fields that no layer reads.
        .with_max_level_hint(span_level);
        let layer: Box<dyn Layer<S> + Send + Sync> = Box::new(
            tracing_opentelemetry::layer()
                .with_tracer(provider.tracer(config.service_name))
                .with_filter(filter),
        );
        layer
    });

    (
        layer,
        Tracing {
            provider,
            state,
            span_level,
            invalid_span_level,
        },
    )
}

/// Returns the collector endpoint to use, preferring the standard OTLP variables
/// over the service's config value. `env` is a parameter so tests can set it.
///
/// Returning `None` disables span export. The exporter builder would otherwise
/// default to `http://localhost:4317` and send spans nowhere (NVBUG 6717563).
fn endpoint(env: impl Fn(&str) -> Option<String>, config_endpoint: Option<&str>) -> Option<String> {
    // Treats an empty endpoint as unset, from a variable or from config. The
    // exporter builder does the same.
    ENDPOINT_VARS
        .iter()
        .find_map(|var| env(var).filter(|endpoint| !endpoint.is_empty()))
        .or_else(|| {
            config_endpoint
                .filter(|endpoint| !endpoint.is_empty())
                .map(str::to_string)
        })
}

/// Reads [`SPAN_LEVEL_VAR`] and returns the level to use, plus any value it could
/// not parse. An unparseable value keeps the default level rather than disabling
/// span export.
fn span_level(
    env: impl Fn(&str) -> Option<String>,
    default: LevelFilter,
) -> (LevelFilter, Option<String>) {
    match env(SPAN_LEVEL_VAR).filter(|value| !value.is_empty()) {
        None => (default, None),
        Some(value) => match value.trim().parse::<LevelFilter>() {
            Ok(level) => (level, None),
            Err(_) => (default, Some(value)),
        },
    }
}

fn build_span_exporter(
    endpoint: &str,
) -> Result<opentelemetry_otlp::SpanExporter, opentelemetry_otlp::ExporterBuildError> {
    // `with_tonic` selects OTLP over gRPC. The builder reads timeout, compression,
    // TLS and headers from the standard `OTEL_EXPORTER_OTLP_*` variables.
    opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
}

/// Calls no `with_sampler`, so the SDK default sampler applies and
/// `OTEL_TRACES_SAMPLER` keeps working.
fn build_tracer_provider(
    exporter: opentelemetry_otlp::SpanExporter,
    service_name: &'static str,
) -> SdkTracerProvider {
    SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(
            Resource::builder()
                .with_attributes([KeyValue::new("service.name", service_name)])
                .build(),
        )
        .build()
}

/// Decides whether the span layer exports a span or event, independently of the
/// log layers. Rejects Tokio runtime spans at every level, because they do not
/// always close and exporting them leaks memory.
fn exportable(span_level: LevelFilter, level: &tracing::Level, module_path: Option<&str>) -> bool {
    *level <= span_level && !module_path.is_some_and(|path| path.starts_with("tokio"))
}

#[cfg(test)]
mod tests {
    use carbide_test_support::value_scenarios;

    use super::*;

    /// The three endpoint sources [`endpoint`] chooses between.
    #[derive(Clone, Copy)]
    struct EndpointInputs {
        traces_var: Option<&'static str>,
        generic_var: Option<&'static str>,
        config: Option<&'static str>,
    }

    fn resolved_endpoint(inputs: EndpointInputs) -> Option<String> {
        endpoint(
            |var| {
                if var == opentelemetry_otlp::OTEL_EXPORTER_OTLP_TRACES_ENDPOINT {
                    inputs.traces_var.map(str::to_string)
                } else if var == opentelemetry_otlp::OTEL_EXPORTER_OTLP_ENDPOINT {
                    inputs.generic_var.map(str::to_string)
                } else {
                    panic!("unexpected endpoint variable: {var}")
                }
            },
            inputs.config,
        )
    }

    #[test]
    fn endpoint_prefers_standard_variables_over_config() {
        // A different value per source, so a failing case names the source that
        // took precedence.
        const TRACES: &str = "http://traces-collector:4317";
        const GENERIC: &str = "http://generic-collector:4317";
        const CONFIG: &str = "http://config-collector:4317";
        const NOTHING_SET: EndpointInputs = EndpointInputs {
            traces_var: None,
            generic_var: None,
            config: None,
        };

        value_scenarios!(
            run = resolved_endpoint;
            "nothing configured leaves span export off" {
                NOTHING_SET => None,
            }

            "the trace-only variable takes precedence over the shared one and the config" {
                EndpointInputs {
                    traces_var: Some(TRACES),
                    generic_var: Some(GENERIC),
                    config: Some(CONFIG),
                    } => Some(TRACES.to_string()),
            }

            "generic variable overrides the config file" {
                EndpointInputs { generic_var: Some(GENERIC), config: Some(CONFIG), ..NOTHING_SET }
                    => Some(GENERIC.to_string()),
            }

            "config file is used when no variable is set" {
                EndpointInputs { config: Some(CONFIG), ..NOTHING_SET }
                    => Some(CONFIG.to_string()),
            }

            "empty config endpoint counts as unset rather than as localhost" {
                EndpointInputs { config: Some(""), ..NOTHING_SET } => None,
            }

            "empty variable falls through instead of shadowing the config" {
                EndpointInputs { traces_var: Some(""), config: Some(CONFIG), ..NOTHING_SET }
                    => Some(CONFIG.to_string()),
            }
        );
    }

    #[test]
    fn span_level_falls_back_to_the_default_on_a_value_it_cannot_parse() {
        fn resolved(value: Option<&'static str>) -> (LevelFilter, Option<String>) {
            span_level(|_| value.map(str::to_string), LevelFilter::INFO)
        }

        value_scenarios!(
            run = resolved;
            "unset keeps the service default" {
                None => (LevelFilter::INFO, None),
            }

            "a level raises span export without touching the log filter" {
                Some("debug") => (LevelFilter::DEBUG, None),
            }

            "surrounding whitespace is tolerated" {
                Some(" trace ") => (LevelFilter::TRACE, None),
            }

            "off disables span export while the exporter stays configured" {
                Some("off") => (LevelFilter::OFF, None),
            }

            "empty counts as unset" {
                Some("") => (LevelFilter::INFO, None),
            }

            "a value it cannot parse is reported and the default level is kept" {
                Some("verbose") => (LevelFilter::INFO, Some("verbose".to_string())),
            }
        );
    }

    // The builder creates the gRPC channel on the current runtime, so this test
    // needs a runtime even though it never connects to a collector.
    #[tokio::test]
    async fn span_exporter_build_validates_endpoint_eagerly() {
        value_scenarios!(
            run = |endpoint| build_span_exporter(endpoint).is_ok();
            "well-formed collector endpoint is accepted" {
                "http://otel-collector.observability.svc.cluster.local:4317" => true,
            }

            "malformed endpoint is rejected at build time rather than at first export" {
                "http://otel collector:4317" => false,
            }
        );
    }

    /// Only a `tracing` macro can construct `Metadata`. This test calls the
    /// filter with the two fields it reads instead.
    #[test]
    fn span_filter_gates_on_its_own_level_and_always_drops_tokio() {
        struct FilterInputs {
            span_level: LevelFilter,
            level: tracing::Level,
            module_path: &'static str,
        }

        fn allows(inputs: FilterInputs) -> bool {
            exportable(inputs.span_level, &inputs.level, Some(inputs.module_path))
        }

        const APP: &str = "carbide_pxe::routes::ipxe";

        value_scenarios!(
            run = allows;
            "a span at the configured level is exported" {
                FilterInputs {
                    span_level: LevelFilter::INFO,
                    level: tracing::Level::INFO,
                    module_path: APP,
                } => true,
            }

            "a span below the configured level is dropped, which keeps DEBUG spans out
             of a default deployment" {
                FilterInputs {
                    span_level: LevelFilter::INFO,
                    level: tracing::Level::DEBUG,
                    module_path: APP,
                } => false,
            }

            "raising the span level exports it, without the log filter being consulted" {
                FilterInputs {
                    span_level: LevelFilter::TRACE,
                    level: tracing::Level::DEBUG,
                    module_path: APP,
                } => true,
            }

            "tokio spans are dropped even at TRACE, because exporting them leaks memory" {
                FilterInputs {
                    span_level: LevelFilter::TRACE,
                    level: tracing::Level::INFO,
                    module_path: "tokio::runtime::task",
                } => false,
            }

            "OFF stops export while the exporter stays configured" {
                FilterInputs {
                    span_level: LevelFilter::OFF,
                    level: tracing::Level::ERROR,
                    module_path: APP,
                } => false,
            }
        );
    }
}
