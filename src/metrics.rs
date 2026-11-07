//! Métricas del gateway en formato de exposición de Prometheus.
//!
//! El conjunto de series es finito y se conoce al arrancar (rutas y backends de
//! `gateway.yaml`), así que el registro se pre-registra en [`Metrics::new`] y
//! cada serie es un `AtomicU64`: no hay `HashMap` ni `Mutex` en la ruta caliente.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::config::GatewayConfig;
use crate::proxy::backend_id;

/// Límites superiores de los buckets del histograma, en segundos.
const BUCKETS: [f64; 11] = [0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0];

const OUTCOMES: [Outcome; 2] = [Outcome::Success, Outcome::AllFailed];
const GLOBAL_OUTCOMES: [GlobalOutcome; 2] =
    [GlobalOutcome::InvalidRequest, GlobalOutcome::ModelNotFound];
const ATTEMPT_RESULTS: [AttemptResult; 6] = [
    AttemptResult::Success,
    AttemptResult::RetryableStatus,
    AttemptResult::TransportError,
    AttemptResult::FirstByteTimeout,
    AttemptResult::MissingApiKey,
    AttemptResult::BodyError,
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
        }
    }
}

/// Histograma acumulativo con buckets fijos.
struct Histogram {
    buckets: [AtomicU64; BUCKETS.len()],
    sum_nanos: AtomicU64,
    count: AtomicU64,
}

impl Histogram {
    fn new() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            sum_nanos: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    /// Registra una observación. Los buckets son acumulativos: se incrementa
    /// cada límite superior mayor o igual que el valor observado.
    fn observe(&self, elapsed: Duration) {
        let secs = elapsed.as_secs_f64();
        for (index, bound) in BUCKETS.iter().enumerate() {
            if secs <= *bound {
                self.buckets[index].fetch_add(1, Ordering::Relaxed);
            }
        }
        self.sum_nanos.fetch_add(
            elapsed.as_nanos().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    fn bucket(&self, index: usize) -> u64 {
        self.buckets[index].load(Ordering::Relaxed)
    }

    fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    fn sum_seconds(&self) -> f64 {
        self.sum_nanos.load(Ordering::Relaxed) as f64 / 1_000_000_000.0
    }
}

struct BackendMetrics {
    id: String,
    attempts: [AtomicU64; 6],
    duration: Histogram,
    /// El backend declara un bloque `warmup`.
    warmup_enabled: bool,
    /// Estado del último warmup: `1` si respondió `2xx`, `0` en caso contrario.
    warm: AtomicU64,
    /// Warmups por resultado: `[success, failure]`.
    warmups: [AtomicU64; 2],
}

struct RouteMetrics {
    name: String,
    /// `[streaming][outcome]`
    requests: [[AtomicU64; 2]; 2],
    failovers: AtomicU64,
    /// Por `streaming`.
    duration: [Histogram; 2],
    backends: Vec<BackendMetrics>,
}

/// Registro de métricas pre-registrado a partir de la configuración.
pub struct Metrics {
    routes: Vec<RouteMetrics>,
    errors: [AtomicU64; 2],
}

impl Metrics {
    /// Pre-registra una serie por ruta, por backend y por resultado posible.
    pub fn new(config: &GatewayConfig) -> Self {
        let routes = config
            .route
            .iter()
            .map(|route| RouteMetrics {
                name: route.name.clone(),
                requests: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0))),
                failovers: AtomicU64::new(0),
                duration: std::array::from_fn(|_| Histogram::new()),
                backends: route
                    .backend
                    .iter()
                    .map(|backend| BackendMetrics {
                        id: backend_id(backend),
                        attempts: std::array::from_fn(|_| AtomicU64::new(0)),
                        duration: Histogram::new(),
                        warmup_enabled: backend.warmup.is_some(),
                        warm: AtomicU64::new(0),
                        warmups: std::array::from_fn(|_| AtomicU64::new(0)),
                    })
                    .collect(),
            })
            .collect();

        Self {
            routes,
            errors: std::array::from_fn(|_| AtomicU64::new(0)),
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
            route.requests[usize::from(streaming)][outcome.index()].fetch_add(1, Ordering::Relaxed);
            route.duration[usize::from(streaming)].observe(elapsed);
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
            backend.attempts[result.index()].fetch_add(1, Ordering::Relaxed);
            backend.duration.observe(elapsed);
        }
    }

    /// Registra el resultado de un warmup: fija el gauge `warm` a `1` (éxito)
    /// o `0` (fallo) e incrementa el counter por resultado.
    pub fn record_warmup(&self, route: usize, backend: usize, ok: bool) {
        if let Some(route) = self.routes.get(route)
            && let Some(backend) = route.backends.get(backend)
        {
            backend.warm.store(u64::from(ok), Ordering::Relaxed);
            backend.warmups[usize::from(!ok)].fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Registra que una petición necesitó más de un intento.
    pub fn record_failover(&self, route: usize) {
        if let Some(route) = self.routes.get(route) {
            route.failovers.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Registra un rechazo anterior a la resolución de ruta.
    pub fn record_global(&self, outcome: GlobalOutcome) {
        self.errors[outcome.index()].fetch_add(1, Ordering::Relaxed);
    }

    /// Renderiza todas las series en formato de exposición de Prometheus.
    pub fn render(&self) -> String {
        let mut out = String::new();

        out.push_str("# HELP llm_gateway_requests_total Peticiones a /v1/chat/completions por ruta y resultado.\n");
        out.push_str("# TYPE llm_gateway_requests_total counter\n");
        for route in &self.routes {
            for streaming in [false, true] {
                for outcome in OUTCOMES {
                    let value = route.requests[usize::from(streaming)][outcome.index()]
                        .load(Ordering::Relaxed);
                    let _ = writeln!(
                        out,
                        "llm_gateway_requests_total{{route=\"{}\",streaming=\"{}\",outcome=\"{}\"}} {value}",
                        escape(&route.name),
                        streaming,
                        outcome.label()
                    );
                }
            }
        }

        out.push_str(
            "# HELP llm_gateway_request_duration_seconds Latencia total de las peticiones (en streaming, tiempo hasta la primera cabecera).\n",
        );
        out.push_str("# TYPE llm_gateway_request_duration_seconds histogram\n");
        for route in &self.routes {
            for streaming in [false, true] {
                let labels = format!(
                    "route=\"{}\",streaming=\"{}\"",
                    escape(&route.name),
                    streaming
                );
                render_histogram_series(
                    &mut out,
                    "llm_gateway_request_duration_seconds",
                    &labels,
                    &route.duration[usize::from(streaming)],
                );
            }
        }

        out.push_str(
            "# HELP llm_gateway_failover_total Peticiones que necesitaron más de un intento.\n",
        );
        out.push_str("# TYPE llm_gateway_failover_total counter\n");
        for route in &self.routes {
            let value = route.failovers.load(Ordering::Relaxed);
            let _ = writeln!(
                out,
                "llm_gateway_failover_total{{route=\"{}\"}} {value}",
                escape(&route.name)
            );
        }

        out.push_str("# HELP llm_gateway_backend_attempts_total Intentos contra cada backend por resultado.\n");
        out.push_str("# TYPE llm_gateway_backend_attempts_total counter\n");
        for route in &self.routes {
            for backend in &route.backends {
                for result in ATTEMPT_RESULTS {
                    let value = backend.attempts[result.index()].load(Ordering::Relaxed);
                    let _ = writeln!(
                        out,
                        "llm_gateway_backend_attempts_total{{route=\"{}\",backend=\"{}\",result=\"{}\"}} {value}",
                        escape(&route.name),
                        escape(&backend.id),
                        result.label()
                    );
                }
            }
        }

        out.push_str("# HELP llm_gateway_backend_duration_seconds Latencia de cada intento contra un backend (en streaming, tiempo hasta la primera cabecera).\n");
        out.push_str("# TYPE llm_gateway_backend_duration_seconds histogram\n");
        for route in &self.routes {
            for backend in &route.backends {
                let labels = format!(
                    "route=\"{}\",backend=\"{}\"",
                    escape(&route.name),
                    escape(&backend.id)
                );
                render_histogram_series(
                    &mut out,
                    "llm_gateway_backend_duration_seconds",
                    &labels,
                    &backend.duration,
                );
            }
        }

        out.push_str(
            "# HELP llm_gateway_errors_total Peticiones rechazadas antes de resolver la ruta.\n",
        );
        out.push_str("# TYPE llm_gateway_errors_total counter\n");
        for outcome in GLOBAL_OUTCOMES {
            let value = self.errors[outcome.index()].load(Ordering::Relaxed);
            let _ = writeln!(
                out,
                "llm_gateway_errors_total{{kind=\"{}\"}} {value}",
                outcome.label()
            );
        }

        // Las familias de warmup solo existen si algún backend declara `warmup`:
        // sin muestras, no se emiten sus directivas `# HELP`/`# TYPE`.
        let any_warmup = self
            .routes
            .iter()
            .any(|route| route.backends.iter().any(|backend| backend.warmup_enabled));

        if any_warmup {
            out.push_str("# HELP llm_gateway_backend_warm Si el último warmup del backend respondió 2xx (1) o no (0).\n");
            out.push_str("# TYPE llm_gateway_backend_warm gauge\n");
            for route in &self.routes {
                for backend in &route.backends {
                    if !backend.warmup_enabled {
                        continue;
                    }
                    let value = backend.warm.load(Ordering::Relaxed);
                    let _ = writeln!(
                        out,
                        "llm_gateway_backend_warm{{route=\"{}\",backend=\"{}\"}} {value}",
                        escape(&route.name),
                        escape(&backend.id)
                    );
                }
            }

            out.push_str(
                "# HELP llm_gateway_warmup_total Warmups ejecutados por backend y resultado.\n",
            );
            out.push_str("# TYPE llm_gateway_warmup_total counter\n");
            for route in &self.routes {
                for backend in &route.backends {
                    if !backend.warmup_enabled {
                        continue;
                    }
                    for (result, index) in [("success", 0usize), ("failure", 1usize)] {
                        let value = backend.warmups[index].load(Ordering::Relaxed);
                        let _ = writeln!(
                            out,
                            "llm_gateway_warmup_total{{route=\"{}\",backend=\"{}\",result=\"{result}\"}} {value}",
                            escape(&route.name),
                            escape(&backend.id)
                        );
                    }
                }
            }
        }

        out
    }
}

/// Emite las líneas `_bucket`, `_sum` y `_count` de una serie de histograma.
fn render_histogram_series(out: &mut String, name: &str, labels: &str, histogram: &Histogram) {
    for (index, bound) in BUCKETS.iter().enumerate() {
        let _ = writeln!(
            out,
            "{name}_bucket{{{labels},le=\"{bound}\"}} {}",
            histogram.bucket(index)
        );
    }
    let count = histogram.count();
    let _ = writeln!(out, "{name}_bucket{{{labels},le=\"+Inf\"}} {count}");
    let _ = writeln!(out, "{name}_sum{{{labels}}} {}", histogram.sum_seconds());
    let _ = writeln!(out, "{name}_count{{{labels}}} {count}");
}

/// Escapa un valor de etiqueta según el formato de exposición de Prometheus.
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Backend, GatewayConfig, Route};

    fn config(route_name: &str) -> GatewayConfig {
        GatewayConfig {
            route: vec![Route {
                name: route_name.to_owned(),
                backend: vec![Backend {
                    base_url: "http://127.0.0.1:1/v1".to_owned(),
                    model: "m".to_owned(),
                    api_key_env: None,
                    timeout_ms: None,
                    warmup: None,
                }],
            }],
        }
    }

    #[test]
    fn histogram_buckets_are_cumulative() {
        let h = Histogram::new();
        h.observe(Duration::from_millis(30));
        h.observe(Duration::from_millis(300));

        assert_eq!(h.bucket(0), 1, "0.05 s solo cuenta la primera");
        assert_eq!(h.bucket(3), 2, "0.5 s cuenta ambas");
        assert_eq!(h.count(), 2);
    }

    #[test]
    fn render_escapes_label_values() {
        let metrics = Metrics::new(&config("a\"b\\c"));
        let text = metrics.render();
        assert!(
            text.contains(r#"route="a\"b\\c""#),
            "etiqueta sin escapar en:\n{text}"
        );
    }

    #[test]
    fn render_emits_zero_series() {
        let metrics = Metrics::new(&config("mech-writer"));
        let text = metrics.render();
        assert!(text.contains("# TYPE llm_gateway_requests_total counter"));
        assert!(text.contains(
            r#"llm_gateway_requests_total{route="mech-writer",streaming="false",outcome="success"} 0"#
        ));
    }

    #[test]
    fn render_inf_matches_count() {
        let metrics = Metrics::new(&config("mech-writer"));
        metrics.record_request(0, false, Outcome::Success, Duration::from_millis(10));
        metrics.record_request(0, false, Outcome::Success, Duration::from_millis(20));

        let text = metrics.render();
        assert!(
            text.contains(r#"llm_gateway_request_duration_seconds_bucket{route="mech-writer",streaming="false",le="+Inf"} 2"#),
            "bucket +Inf:\n{text}"
        );
        assert!(
            text.contains(r#"llm_gateway_request_duration_seconds_count{route="mech-writer",streaming="false"} 2"#),
            "count:\n{text}"
        );
    }

    #[test]
    fn render_escapes_newline() {
        let metrics = Metrics::new(&config("a\nb"));
        let text = metrics.render();
        assert!(
            text.contains(r#"route="a\nb""#),
            "salto de línea sin escapar:\n{text}"
        );
    }
}
