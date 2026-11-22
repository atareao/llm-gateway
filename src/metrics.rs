//! Métricas del gateway sobre instrumentos OpenTelemetry.
//!
//! El conjunto de series es finito y se conoce al arrancar (rutas y backends de
//! `gateway.yaml`), así que en [`Metrics::new`] se **pre-registran** los
//! conjuntos de atributos de cada serie: la ruta caliente solo recorre índices
//! y llama a `add`/`record`, sin `HashMap` ni `Mutex`.
//!
//! Los instrumentos viven en el `MeterProvider` global instalado por
//! [`crate::telemetry`], que lleva dos readers: el `PrometheusExporter`
//! (siempre; sirve `GET /metrics`) y, si `signals.metrics`, un `PeriodicReader`
//! que los exporta por OTLP. Una única fuente de verdad para scrape y export.
//!
//! Los nombres de serie `llm_gateway_*` son contrato observable: los counters
//! se nombran **sin** el sufijo `_total` porque el `PrometheusExporter` lo
//! añade al renderizar.

use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge, Histogram};
use prometheus::Encoder as _;

use crate::config::GatewayConfig;
use crate::proxy::backend_id;
use crate::telemetry;

/// Límites superiores de los buckets del histograma, en segundos.
const BUCKETS: [f64; 11] = [0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0];

/// Nombre del `Meter` (scope de instrumentación).
const SCOPE_NAME: &str = "llm-gateway";

const OUTCOMES: [Outcome; 2] = [Outcome::Success, Outcome::AllFailed];
const GLOBAL_OUTCOMES: [GlobalOutcome; 2] =
    [GlobalOutcome::InvalidRequest, GlobalOutcome::ModelNotFound];
const ATTEMPT_RESULTS: [AttemptResult; 7] = [
    AttemptResult::Success,
    AttemptResult::RetryableStatus,
    AttemptResult::TransportError,
    AttemptResult::FirstByteTimeout,
    AttemptResult::MissingApiKey,
    AttemptResult::BodyError,
    AttemptResult::NoToolCall,
];

/// Resultado final de una petición a `/v1/chat/completions`.
#[derive(Clone, Copy, Debug)]
pub enum Outcome {
    Success,
    AllFailed,
}

impl Outcome {
    /// Valor de la etiqueta `outcome`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::AllFailed => "all_failed",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Success => 0,
            Self::AllFailed => 1,
        }
    }
}

/// Error detectado antes de resolver la ruta.
#[derive(Clone, Copy, Debug)]
pub enum GlobalOutcome {
    InvalidRequest,
    ModelNotFound,
}

impl GlobalOutcome {
    /// Valor de la etiqueta `kind`.
    pub fn label(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::ModelNotFound => "model_not_found",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::InvalidRequest => 0,
            Self::ModelNotFound => 1,
        }
    }
}

/// Resultado de un intento contra un backend.
#[derive(Clone, Copy, Debug)]
pub enum AttemptResult {
    Success,
    RetryableStatus,
    TransportError,
    FirstByteTimeout,
    MissingApiKey,
    BodyError,
    /// Respuesta `2xx` sin `tool_calls` cuando la petición los exigía.
    NoToolCall,
}

impl AttemptResult {
    /// Valor de la etiqueta `result`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::RetryableStatus => "retryable_status",
            Self::TransportError => "transport_error",
            Self::FirstByteTimeout => "first_byte_timeout",
            Self::MissingApiKey => "missing_api_key",
            Self::BodyError => "body_error",
            Self::NoToolCall => "no_tool_call",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Success => 0,
            Self::RetryableStatus => 1,
            Self::TransportError => 2,
            Self::FirstByteTimeout => 3,
            Self::MissingApiKey => 4,
            Self::BodyError => 5,
            Self::NoToolCall => 6,
        }
    }
}

/// Atributos pre-registrados de un backend.
struct BackendMetrics {
    /// `[result]` → atributos `route`, `backend`, `result`.
    attempts: [Vec<KeyValue>; 7],
    /// Atributos `route`, `backend`.
    duration: Vec<KeyValue>,
    /// El backend declara un bloque `warmup`.
    warmup_enabled: bool,
    /// `[success, failure]` → atributos `route`, `backend`, `result`.
    warmups: [Vec<KeyValue>; 2],
}

/// Atributos pre-registrados de una ruta.
struct RouteMetrics {
    /// `[streaming][outcome]` → atributos `route`, `streaming`, `outcome`.
    requests: [[Vec<KeyValue>; 2]; 2],
    /// Atributos `route`.
    failover: Vec<KeyValue>,
    /// `[streaming]` → atributos `route`, `streaming`.
    duration: [Vec<KeyValue>; 2],
    backends: Vec<BackendMetrics>,
}

/// Registro de métricas pre-registrado a partir de la configuración.
pub struct Metrics {
    requests: Counter<u64>,
    request_duration: Histogram<f64>,
    failover: Counter<u64>,
    backend_attempts: Counter<u64>,
    backend_duration: Histogram<f64>,
    errors: Counter<u64>,
    backend_warm: Gauge<u64>,
    warmup: Counter<u64>,
    routes: Vec<RouteMetrics>,
    /// `[kind]` → atributos `kind`.
    error_kinds: [Vec<KeyValue>; 2],
}

/// Atributos `route`, `streaming`, `outcome`.
fn request_attrs(route: &str, streaming: bool, outcome: Outcome) -> Vec<KeyValue> {
    vec![
        KeyValue::new("route", route.to_owned()),
        KeyValue::new("streaming", streaming.to_string()),
        KeyValue::new("outcome", outcome.label()),
    ]
}

/// Atributos `route`, `streaming`.
fn duration_attrs(route: &str, streaming: bool) -> Vec<KeyValue> {
    vec![
        KeyValue::new("route", route.to_owned()),
        KeyValue::new("streaming", streaming.to_string()),
    ]
}

impl Metrics {
    /// Pre-registra una serie por ruta, por backend y por resultado posible.
    ///
    /// Se registra un `0` en cada serie pre-registrada para que `/metrics`
    /// emita el conjunto completo de series desde el primer scrape, igual que
    /// hacía el registro de atómicos.
    pub fn new(config: &GatewayConfig) -> Self {
        // Sin `telemetry::init` (tests, usos embebidos) garantiza al menos el
        // reader Prometheus para que `/metrics` funcione.
        telemetry::ensure_meter_provider();
        let meter = opentelemetry::global::meter(SCOPE_NAME);

        let requests = meter
            .u64_counter("llm_gateway_requests")
            .with_description("Peticiones a /v1/chat/completions por ruta y resultado.")
            .build();
        let request_duration = meter
            .f64_histogram("llm_gateway_request_duration_seconds")
            .with_description(
                "Latencia total de las peticiones (en streaming, tiempo hasta la primera cabecera).",
            )
            .with_boundaries(BUCKETS.to_vec())
            .build();
        let failover = meter
            .u64_counter("llm_gateway_failover")
            .with_description("Peticiones que necesitaron más de un intento.")
            .build();
        let backend_attempts = meter
            .u64_counter("llm_gateway_backend_attempts")
            .with_description("Intentos contra cada backend por resultado.")
            .build();
        let backend_duration = meter
            .f64_histogram("llm_gateway_backend_duration_seconds")
            .with_description(
                "Latencia de cada intento contra un backend (en streaming, tiempo hasta la primera cabecera).",
            )
            .with_boundaries(BUCKETS.to_vec())
            .build();
        let errors = meter
            .u64_counter("llm_gateway_errors")
            .with_description("Peticiones rechazadas antes de resolver la ruta.")
            .build();
        let backend_warm = meter
            .u64_gauge("llm_gateway_backend_warm")
            .with_description("Si el último warmup del backend respondió 2xx (1) o no (0).")
            .build();
        let warmup = meter
            .u64_counter("llm_gateway_warmup")
            .with_description("Warmups ejecutados por backend y resultado.")
            .build();

        let error_kinds = std::array::from_fn(|index| {
            vec![KeyValue::new("kind", GLOBAL_OUTCOMES[index].label())]
        });
        for attrs in &error_kinds {
            errors.add(0, attrs);
        }

        let routes = config
            .route
            .iter()
            .map(|route| {
                let requests = std::array::from_fn(|streaming| {
                    std::array::from_fn(|outcome| {
                        request_attrs(&route.name, streaming == 1, OUTCOMES[outcome])
                    })
                });
                let failover = vec![KeyValue::new("route", route.name.clone())];
                let duration =
                    std::array::from_fn(|streaming| duration_attrs(&route.name, streaming == 1));
                let backends = route
                    .backend
                    .iter()
                    .map(|backend| {
                        let id = backend_id(backend);
                        let attempts = std::array::from_fn(|result| {
                            vec![
                                KeyValue::new("route", route.name.clone()),
                                KeyValue::new("backend", id.clone()),
                                KeyValue::new("result", ATTEMPT_RESULTS[result].label()),
                            ]
                        });
                        let duration = vec![
                            KeyValue::new("route", route.name.clone()),
                            KeyValue::new("backend", id.clone()),
                        ];
                        let warmups = std::array::from_fn(|result| {
                            vec![
                                KeyValue::new("route", route.name.clone()),
                                KeyValue::new("backend", id.clone()),
                                KeyValue::new(
                                    "result",
                                    if result == 0 { "success" } else { "failure" },
                                ),
                            ]
                        });
                        BackendMetrics {
                            attempts,
                            duration,
                            warmup_enabled: backend.warmup.is_some(),
                            warmups,
                        }
                    })
                    .collect();
                RouteMetrics {
                    requests,
                    failover,
                    duration,
                    backends,
                }
            })
            .collect::<Vec<_>>();

        // Pre-registro: emite las series en cero desde el primer scrape.
        for route in &routes {
            for attrs in route.requests.iter().flatten() {
                requests.add(0, attrs);
            }
            failover.add(0, &route.failover);
            for backend in &route.backends {
                for attrs in &backend.attempts {
                    backend_attempts.add(0, attrs);
                }
                if backend.warmup_enabled {
                    backend_warm.record(0, &backend.duration);
                    for attrs in &backend.warmups {
                        warmup.add(0, attrs);
                    }
                }
            }
        }

        Self {
            requests,
            request_duration,
            failover,
            backend_attempts,
            backend_duration,
            errors,
            backend_warm,
            warmup,
            routes,
            error_kinds,
        }
    }

    /// Registra el resultado final de una petición y su latencia total.
    pub fn record_request(
        &self,
        route: usize,
        streaming: bool,
        outcome: Outcome,
        elapsed: Duration,
    ) {
        if let Some(route) = self.routes.get(route) {
            let streaming = usize::from(streaming);
            self.requests
                .add(1, &route.requests[streaming][outcome.index()]);
            self.request_duration
                .record(elapsed.as_secs_f64(), &route.duration[streaming]);
        }
    }

    /// Registra el resultado de un intento contra un backend y su latencia.
    pub fn record_attempt(
        &self,
        route: usize,
        backend: usize,
        result: AttemptResult,
        elapsed: Duration,
    ) {
        if let Some(route) = self.routes.get(route)
            && let Some(backend) = route.backends.get(backend)
        {
            self.backend_attempts
                .add(1, &backend.attempts[result.index()]);
            self.backend_duration
                .record(elapsed.as_secs_f64(), &backend.duration);
        }
    }

    /// Registra el resultado de un warmup: fija el gauge `warm` a `1` (éxito)
    /// o `0` (fallo) e incrementa el counter por resultado.
    pub fn record_warmup(&self, route: usize, backend: usize, ok: bool) {
        if let Some(route) = self.routes.get(route)
            && let Some(backend) = route.backends.get(backend)
        {
            self.backend_warm.record(u64::from(ok), &backend.duration);
            self.warmup.add(1, &backend.warmups[usize::from(!ok)]);
        }
    }

    /// Registra que una petición necesitó más de un intento.
    pub fn record_failover(&self, route: usize) {
        if let Some(route) = self.routes.get(route) {
            self.failover.add(1, &route.failover);
        }
    }

    /// Registra un rechazo anterior a la resolución de ruta.
    pub fn record_global(&self, outcome: GlobalOutcome) {
        self.errors.add(1, &self.error_kinds[outcome.index()]);
    }

    /// Renderiza todas las series en formato de exposición de Prometheus.
    ///
    /// Lee del mismo registro OTel que alimenta la exportación OTLP, así que
    /// scrape y export comparten una única fuente de verdad.
    pub fn render(&self) -> String {
        let families = telemetry::prometheus_registry().gather();
        let mut buffer = Vec::new();
        if let Err(err) = prometheus::TextEncoder::new().encode(&families, &mut buffer) {
            tracing::error!(error = %err, "no se pudieron codificar las métricas");
            return String::new();
        }
        String::from_utf8_lossy(&buffer).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Backend, GatewayConfig, Route, Warmup};

    /// Construye una config con una ruta y un backend; si `warmup`, lo declara.
    ///
    /// El nombre de ruta es único por test: los instrumentos viven en el
    /// `MeterProvider` global compartido por todo el proceso, así que la
    /// aislamiento entre tests se consigue con conjuntos de etiquetas únicos.
    fn config(route_name: &str, warmup: bool) -> GatewayConfig {
        GatewayConfig {
            route: vec![Route {
                name: route_name.to_owned(),
                backend: vec![Backend {
                    kind: crate::config::BackendKind::Static,
                    base_url: "http://127.0.0.1:1/v1".to_owned(),
                    model: Some("m".to_owned()),
                    api_key_env: None,
                    timeout_ms: None,
                    profile: None,
                    discovery: None,
                    warmup: warmup.then_some(Warmup {
                        interval_ms: 1_000,
                        timeout_ms: None,
                    }),
                }],
            }],
            observability: None,
        }
    }

    /// Línea de `text` para la serie `name` con `route="<route>"`, si existe.
    fn line_for<'a>(text: &'a str, name: &str, route: &str) -> Option<&'a str> {
        text.lines()
            .find(|line| line.starts_with(name) && line.contains(&format!(r#"route="{route}""#)))
    }

    /// Valor (último campo) de una línea de serie.
    fn value(line: &str) -> &str {
        line.rsplit(' ').next().unwrap_or_default()
    }

    /// Valor de una serie o `None` si la serie no está presente.
    fn series_value<'a>(text: &'a str, name: &str, route: &str) -> Option<&'a str> {
        line_for(text, name, route).map(value)
    }

    #[test]
    fn render_emits_zero_series_from_the_first_scrape() {
        let metrics = Metrics::new(&config("scrape-vacio", false));
        let text = metrics.render();

        assert!(
            text.contains("# TYPE llm_gateway_requests_total counter"),
            "falta el TYPE del counter:\n{text}"
        );
        let lines: Vec<&str> = text
            .lines()
            .filter(|line| {
                line.starts_with("llm_gateway_requests_total")
                    && line.contains(r#"route="scrape-vacio""#)
            })
            .collect();
        assert_eq!(
            lines.len(),
            4,
            "deben emitirse las 4 combinaciones:\n{text}"
        );
        assert!(
            lines.iter().all(|line| value(line) == "0"),
            "todas las series deben estar en cero:\n{text}"
        );
    }

    #[test]
    fn render_keeps_the_llm_gateway_series_names() {
        let metrics = Metrics::new(&config("nombres-de-serie", false));
        let text = metrics.render();

        for name in [
            "llm_gateway_requests_total",
            "llm_gateway_failover_total",
            "llm_gateway_backend_attempts_total",
            "llm_gateway_errors_total",
        ] {
            assert!(text.contains(name), "falta la serie {name}:\n{text}");
        }
    }

    #[test]
    fn render_escapes_label_values() {
        let metrics = Metrics::new(&config("a\"b\\c", false));
        let text = metrics.render();
        assert!(
            text.contains(r#"route="a\"b\\c""#),
            "etiqueta sin escapar en:\n{text}"
        );
    }

    #[test]
    fn render_escapes_newline() {
        let metrics = Metrics::new(&config("a\nb", false));
        let text = metrics.render();
        assert!(
            text.contains(r#"route="a\nb""#),
            "salto de línea sin escapar:\n{text}"
        );
    }

    #[test]
    fn histogram_buckets_are_cumulative() {
        let route = "buckets-acumulativos";
        let metrics = Metrics::new(&config(route, false));
        metrics.record_request(0, false, Outcome::Success, Duration::from_millis(30));
        metrics.record_request(0, false, Outcome::Success, Duration::from_millis(300));

        let text = metrics.render();
        let bucket = |bound: &str| {
            text.lines()
                .find(|line| {
                    line.starts_with("llm_gateway_request_duration_seconds_bucket")
                        && line.contains(&format!(r#"route="{route}""#))
                        && line.contains(&format!(r#"le="{bound}""#))
                })
                .map(value)
        };

        assert_eq!(
            bucket("0.05"),
            Some("1"),
            "0.05 s solo cuenta la primera:\n{text}"
        );
        assert_eq!(bucket("0.5"), Some("2"), "0.5 s cuenta ambas:\n{text}");
    }

    #[test]
    fn render_inf_matches_count() {
        let route = "inf-y-count";
        let metrics = Metrics::new(&config(route, false));
        metrics.record_request(0, false, Outcome::Success, Duration::from_millis(10));
        metrics.record_request(0, false, Outcome::Success, Duration::from_millis(20));

        let text = metrics.render();
        let inf = text
            .lines()
            .find(|line| {
                line.starts_with("llm_gateway_request_duration_seconds_bucket")
                    && line.contains(&format!(r#"route="{route}""#))
                    && line.contains(r#"le="+Inf""#)
            })
            .map(value);
        assert_eq!(inf, Some("2"), "bucket +Inf:\n{text}");
        assert_eq!(
            series_value(&text, "llm_gateway_request_duration_seconds_count", route),
            Some("2"),
            "count:\n{text}"
        );
    }

    #[test]
    fn requests_counter_increments_by_route_streaming_and_outcome() {
        let route = "counter-por-combinacion";
        let metrics = Metrics::new(&config(route, false));
        metrics.record_request(0, false, Outcome::Success, Duration::from_millis(5));
        metrics.record_request(0, true, Outcome::AllFailed, Duration::from_millis(5));

        let text = metrics.render();
        let series = |streaming: &str, outcome: &str| {
            text.lines()
                .find(|line| {
                    line.starts_with("llm_gateway_requests_total")
                        && line.contains(&format!(r#"route="{route}""#))
                        && line.contains(&format!(r#"streaming="{streaming}""#))
                        && line.contains(&format!(r#"outcome="{outcome}""#))
                })
                .map(value)
        };

        assert_eq!(
            series("false", "success"),
            Some("1"),
            "success no-stream:\n{text}"
        );
        assert_eq!(
            series("true", "all_failed"),
            Some("1"),
            "all_failed streaming:\n{text}"
        );
        assert_eq!(
            series("true", "success"),
            Some("0"),
            "resto en cero:\n{text}"
        );
        assert_eq!(
            series("false", "all_failed"),
            Some("0"),
            "resto en cero:\n{text}"
        );
    }

    #[test]
    fn global_outcomes_are_recorded_by_kind() {
        let metrics = Metrics::new(&config("outcome-global", false));
        metrics.record_global(GlobalOutcome::ModelNotFound);

        let text = metrics.render();
        let line = text
            .lines()
            .find(|line| {
                line.starts_with("llm_gateway_errors_total")
                    && line.contains(r#"kind="model_not_found""#)
            })
            .expect("errors_total con kind=model_not_found");
        assert_eq!(value(line), "1", "errors_total:\n{text}");
    }

    #[test]
    fn warmup_families_only_exist_for_backends_that_declare_warmup() {
        let route = "sin-warmup";
        let metrics = Metrics::new(&config(route, false));
        let text = metrics.render();

        for name in ["llm_gateway_backend_warm", "llm_gateway_warmup_total"] {
            assert!(
                line_for(&text, name, route).is_none(),
                "{name} no debe tener series para una ruta sin warmup:\n{text}"
            );
        }
    }

    #[test]
    fn warmup_gauge_and_counter_are_recorded() {
        let route = "con-warmup";
        let metrics = Metrics::new(&config(route, true));
        metrics.record_warmup(0, 0, true);
        metrics.record_warmup(0, 0, false);

        let text = metrics.render();
        assert_eq!(
            series_value(&text, "llm_gateway_backend_warm", route),
            Some("0"),
            "el gauge warm debe quedar a 0 tras el fallo:\n{text}"
        );

        let warmup = |result: &str| {
            text.lines()
                .find(|line| {
                    line.starts_with("llm_gateway_warmup_total")
                        && line.contains(&format!(r#"route="{route}""#))
                        && line.contains(&format!(r#"result="{result}""#))
                })
                .map(value)
        };
        assert_eq!(warmup("success"), Some("1"), "warmup success:\n{text}");
        assert_eq!(warmup("failure"), Some("1"), "warmup failure:\n{text}");
    }

    #[test]
    fn attempt_results_are_recorded_per_backend() {
        let route = "attempts-por-backend";
        let metrics = Metrics::new(&config(route, false));
        metrics.record_attempt(
            0,
            0,
            AttemptResult::TransportError,
            Duration::from_millis(2),
        );
        metrics.record_attempt(0, 0, AttemptResult::Success, Duration::from_millis(3));

        let text = metrics.render();
        let attempt = |result: &str| {
            text.lines()
                .find(|line| {
                    line.starts_with("llm_gateway_backend_attempts_total")
                        && line.contains(&format!(r#"route="{route}""#))
                        && line.contains(&format!(r#"result="{result}""#))
                })
                .map(value)
        };
        assert_eq!(
            attempt("transport_error"),
            Some("1"),
            "intento fallido:\n{text}"
        );
        assert_eq!(attempt("success"), Some("1"), "intento con éxito:\n{text}");
    }

    #[test]
    fn failover_counter_is_recorded_per_route() {
        let route = "failover-por-ruta";
        let metrics = Metrics::new(&config(route, false));
        metrics.record_failover(0);

        let text = metrics.render();
        assert_eq!(
            series_value(&text, "llm_gateway_failover_total", route),
            Some("1"),
            "failover_total:\n{text}"
        );
    }
}
