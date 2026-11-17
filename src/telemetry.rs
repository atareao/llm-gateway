//! Inicialización de la telemetría del gateway (OTLP + Prometheus).
//!
//! El módulo centraliza tres cosas:
//!
//! - El **registro de métricas** (`PrometheusExporter` sobre un
//!   `prometheus::Registry` global) que sirve `GET /metrics` y, si
//!   `signals.metrics`, un `PeriodicReader` que las exporta por OTLP.
//! - La **exportación de logs** (`opentelemetry-appender-tracing`) cuando
//!   `signals.logs`.
//! - La **exportación de trazas** (`tracing-opentelemetry`) y el propagador
//!   W3C Trace Context cuando `signals.traces`.
//!
//! Sin el bloque `observability` el gateway sigue instalando el
//! `PrometheusExporter` (es la fuente única de `/metrics`) pero **no** añade
//! ningún exportador OTLP: cero cambio de comportamiento.
//!
//! ## Orden de llamada
//!
//! [`init`] debe llamarse **antes** de construir [`crate::proxy::AppState`],
//! porque `Metrics` toma sus instrumentos del `MeterProvider` global.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use anyhow::{Context as _, Result};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{WithExportConfig as _, WithHttpConfig as _};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{SdkTracerProvider, Tracer};
use prometheus::Registry;

use crate::config::{GatewayConfig, Otlp};

/// Intervalo por defecto de exportación OTLP de métricas y logs (15 s).
pub const DEFAULT_OTLP_INTERVAL_MS: u64 = 15_000;

/// Timeout por defecto de cada exportación OTLP (10 s, el del stack OTel).
pub const DEFAULT_OTLP_TIMEOUT_MS: u64 = 10_000;

/// Nombre del `Meter`/`Tracer` (scope de instrumentación).
const SCOPE_NAME: &str = "llm-gateway";

/// Nombre de servicio por defecto reportado como recurso OTel.
const DEFAULT_SERVICE_NAME: &str = "llm-gateway";

/// Registro Prometheus global, compartido por el `PrometheusExporter` y
/// `GET /metrics`.
static PROMETHEUS_REGISTRY: OnceLock<Registry> = OnceLock::new();

/// `MeterProvider` global, construido **como mucho una vez**.
///
/// El `PrometheusExporter` registra su collector en el registro Prometheus al
/// construirse, y registrarlo dos veces falla (`Duplicate metrics collector
/// registration attempted`). Por eso el provider completo (Prometheus +
/// opcionalmente el `PeriodicReader` OTLP) se memoiza: [`init`] y
/// [`ensure_meter_provider`] reutilizan el mismo en vez de construir otro.
static METER_PROVIDER: OnceLock<SdkMeterProvider> = OnceLock::new();

/// `true` si el `MeterProvider` memoizado tiene reader OTLP de métricas.
static OTLP_METRICS_INSTALLED: AtomicBool = AtomicBool::new(false);

/// Serializa la construcción e instalación del `MeterProvider` para que no se
/// registren dos `PrometheusExporter` sobre el mismo registro (registrar dos
/// veces el mismo collector falla y dejaría `/metrics` sin datos).
static METER_INIT_LOCK: Mutex<()> = Mutex::new(());

/// Devuelve el registro Prometheus global, creándolo si es la primera vez.
pub fn prometheus_registry() -> Registry {
    PROMETHEUS_REGISTRY.get_or_init(Registry::new).clone()
}

/// Toma el lock de inicialización del `MeterProvider`, ignorando el envenenado.
fn lock_meter_init() -> MutexGuard<'static, ()> {
    METER_INIT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Devuelve el `MeterProvider` global construyéndolo como mucho una vez, junto
/// con si tiene reader OTLP de métricas.
///
/// Idempotente y seguro ante llamadas concurrentes: el primero que llega lo
/// construye con `otlp` (Prometheus siempre; reader OTLP si `signals.metrics`)
/// y los siguientes reutilizan ese mismo provider, ignorando `otlp`. Cuando se
/// reutiliza un provider ya construido sin OTLP, no hay forma de añadírselo
/// (un provider no admite readers nuevos tras `build`).
///
/// # Errors
/// Devuelve error si el constructor del exportador Prometheus falla.
fn meter_provider(otlp: Option<&Otlp>) -> Result<(SdkMeterProvider, bool)> {
    let _guard = lock_meter_init();
    if let Some(provider) = METER_PROVIDER.get() {
        return Ok((
            provider.clone(),
            OTLP_METRICS_INSTALLED.load(Ordering::Acquire),
        ));
    }

    let (provider, otlp_metrics) = build_meter_provider(otlp)?;
    OTLP_METRICS_INSTALLED.store(otlp_metrics, Ordering::Release);
    let _ = METER_PROVIDER.set(provider.clone());
    opentelemetry::global::set_meter_provider(provider.clone());
    Ok((provider, otlp_metrics))
}

/// Garantiza que exista un `MeterProvider` global (solo Prometheus) cuando
/// [`init`] no se ha llamado: lo usan los tests y cualquier uso de `Metrics`
/// sin telemetría. Idempotente y seguro ante llamadas concurrentes.
pub fn ensure_meter_provider() {
    if METER_PROVIDER.get().is_some() {
        return;
    }
    if let Err(err) = meter_provider(None) {
        tracing::error!(error = %err, "no se pudo instalar el MeterProvider");
    }
}

/// Guarda RAII de la telemetría: mantiene vivos los providers, vacía las
/// señales pendientes en el apagado ordenado y garantiza ese flush incluso si
/// el servidor no llega a arrancar (gracias a [`Drop`]).
///
/// Los campos son opcionales porque solo se construyen los que pide la
/// configuración: el de métricas existe siempre (sirve `/metrics`), los de logs
/// y trazas solo si sus señales están activas.
#[derive(Default)]
pub struct TelemetryGuard {
    /// Provider de métricas (siempre presente: sirve `/metrics`).
    meter: Option<SdkMeterProvider>,
    /// Provider de logs OTLP (solo si `signals.logs`).
    logger: Option<SdkLoggerProvider>,
    /// Provider de trazas OTLP (solo si `signals.traces`).
    tracer: Option<SdkTracerProvider>,
    /// `true` si hay un reader OTLP de métricas activo.
    otlp_metrics: bool,
    /// `true` una vez vaciados los providers: hace que `flush` sea idempotente
    /// (lo llaman tanto el apagado ordenado de `main` como [`Drop`]).
    flushed: AtomicBool,
}

impl TelemetryGuard {
    /// `true` si algún exportador OTLP está activo (métricas, logs o trazas).
    ///
    /// API de **diagnóstico** (arranque y tests): no la usa la lógica de
    /// producción.
    pub fn exports_otlp(&self) -> bool {
        self.otlp_metrics || self.logger.is_some() || self.tracer.is_some()
    }

    /// Fuerza el flush y el shutdown de todos los providers, de forma que las
    /// señales pendientes no se pierdan en el apagado.
    ///
    /// Idempotente: a partir de la segunda llamada es un no-op, así que es
    /// seguro llamarlo explícitamente y dejarlo también a [`Drop`].
    pub fn flush(&self) {
        if self.flushed.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(provider) = &self.meter {
            flush_metrics(provider);
        }
        if let Some(provider) = &self.logger {
            flush_logs(provider);
        }
        if let Some(provider) = &self.tracer {
            flush_traces(provider);
        }
    }
}

impl Drop for TelemetryGuard {
    /// Red de seguridad del apagado ordenado: si `main` no llega a llamar a
    /// [`TelemetryGuard::flush`] (por ejemplo, el bind o `axum::serve` fallan
    /// antes), el flush se hace igualmente aquí.
    fn drop(&mut self) {
        self.flush();
    }
}

/// Inicializa la telemetría a partir de la configuración.
///
/// Instala el `MeterProvider` global (con `PrometheusExporter` y, si
/// `signals.metrics`, un `PeriodicReader` OTLP), los providers de logs y trazas
/// según `signals`, el propagador W3C y el subscriber de `tracing`.
///
/// # Errors
/// Devuelve error si algún exportador no se puede construir (endpoint
/// inválido, TLS, etc.).
pub fn init(config: &GatewayConfig) -> Result<TelemetryGuard> {
    let otlp = config
        .observability
        .as_ref()
        .and_then(|obs| obs.otlp.as_ref());

    // Se reutiliza el provider global si ya existe (p. ej. porque un consumidor
    // llamó a `ensure_meter_provider` al crear `Metrics`): construir otro
    // intentaría registrar de nuevo el `PrometheusExporter` y fallaría.
    let (meter_provider, otlp_metrics) = meter_provider(otlp)?;
    if otlp.is_some_and(|otlp| otlp.signals.metrics) && !otlp_metrics {
        tracing::warn!(
            "el MeterProvider global ya estaba instalado sin reader OTLP; las métricas no se exportarán por OTLP"
        );
    }

    let logger_provider = match otlp.filter(|otlp| otlp.signals.logs) {
        Some(otlp) => Some(build_logger_provider(otlp)?),
        None => None,
    };

    let (tracer_provider, tracer) = match otlp.filter(|otlp| otlp.signals.traces) {
        Some(otlp) => {
            let (provider, tracer) = build_tracer_provider(otlp)?;
            opentelemetry::global::set_tracer_provider(provider.clone());
            opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
            (Some(provider), Some(tracer))
        }
        None => (None, None),
    };

    install_tracing_subscriber(&logger_provider, tracer.as_ref())?;

    Ok(TelemetryGuard {
        meter: Some(meter_provider),
        logger: logger_provider,
        tracer: tracer_provider,
        otlp_metrics,
        flushed: AtomicBool::new(false),
    })
}

/// Construye el `MeterProvider` con el `PrometheusExporter` (siempre) y, si hay
/// `otlp` con `signals.metrics`, un `PeriodicReader` OTLP.
fn build_meter_provider(otlp: Option<&Otlp>) -> Result<(SdkMeterProvider, bool)> {
    let exporter = opentelemetry_prometheus::exporter()
        .with_registry(prometheus_registry())
        // El contrato observable de /metrics son solo las series `llm_gateway_*`.
        .without_target_info()
        .scope_info_enabled(false)
        .build()
        .context("no se pudo construir el PrometheusExporter")?;

    let mut builder = SdkMeterProvider::builder()
        .with_reader(exporter)
        .with_resource(resource(otlp));

    let mut otlp_metrics = false;
    if let Some(otlp) = otlp.filter(|otlp| otlp.signals.metrics) {
        let reader = PeriodicReader::builder(metric_exporter(otlp)?)
            .with_interval(interval(otlp.interval_ms))
            .build();
        builder = builder.with_reader(reader);
        otlp_metrics = true;
    }

    Ok((builder.build(), otlp_metrics))
}

/// Construye el `SdkLoggerProvider` con exportador OTLP por lotes.
fn build_logger_provider(otlp: &Otlp) -> Result<SdkLoggerProvider> {
    let exporter = opentelemetry_otlp::LogExporter::builder()
        .with_http()
        .with_endpoint(signal_url(otlp, "logs"))
        .with_timeout(timeout(otlp.timeout_ms))
        .with_headers(headers(otlp))
        .build()
        .context("no se pudo construir el LogExporter OTLP")?;

    Ok(SdkLoggerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource(Some(otlp)))
        .build())
}

/// Construye el `SdkTracerProvider` con exportador OTLP por lotes y un
/// `Tracer` listo para el layer de `tracing`.
fn build_tracer_provider(otlp: &Otlp) -> Result<(SdkTracerProvider, Tracer)> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(signal_url(otlp, "traces"))
        .with_timeout(timeout(otlp.timeout_ms))
        .with_headers(headers(otlp))
        .build()
        .context("no se pudo construir el SpanExporter OTLP")?;

    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource(Some(otlp)))
        .build();
    let tracer = provider.tracer(SCOPE_NAME);
    Ok((provider, tracer))
}

/// Exportador OTLP de métricas por HTTP.
fn metric_exporter(otlp: &Otlp) -> Result<opentelemetry_otlp::MetricExporter> {
    opentelemetry_otlp::MetricExporter::builder()
        .with_http()
        .with_endpoint(signal_url(otlp, "metrics"))
        .with_timeout(timeout(otlp.timeout_ms))
        .with_headers(headers(otlp))
        .build()
        .context("no se pudo construir el MetricExporter OTLP")
}

/// Recurso OTel compartido por los providers (nombre de servicio).
fn resource(otlp: Option<&Otlp>) -> Resource {
    let name = otlp
        .and_then(|otlp| otlp.service_name.as_deref())
        .unwrap_or(DEFAULT_SERVICE_NAME);
    Resource::builder()
        .with_service_name(name.to_owned())
        .build()
}

/// Señales OTLP que la configuración pide exportar, en orden fijo
/// (`metrics`, `logs`, `traces`).
///
/// Función **pura**: no lee variables de entorno, no instala providers y por
/// tanto es segura de llamar en tests que no deben tocar el estado global. Sin
/// bloque `observability.otlp` devuelve un vector vacío.
///
/// API de **diagnóstico** (arranque y tests): no la usa la lógica de producción.
pub fn planned_exporters(config: &GatewayConfig) -> Vec<&'static str> {
    let Some(otlp) = config
        .observability
        .as_ref()
        .and_then(|obs| obs.otlp.as_ref())
    else {
        return Vec::new();
    };

    let mut exporters = Vec::new();
    if otlp.signals.metrics {
        exporters.push("metrics");
    }
    if otlp.signals.logs {
        exporters.push("logs");
    }
    if otlp.signals.traces {
        exporters.push("traces");
    }
    exporters
}

/// Instala el subscriber de `tracing`: salida local (texto o JSON) más los
/// layers OTLP de logs y trazas cuando proceda.
fn install_tracing_subscriber(
    logger: &Option<SdkLoggerProvider>,
    tracer: Option<&Tracer>,
) -> Result<()> {
    use tracing_subscriber::layer::SubscriberExt as _;

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let json = std::env::var("GATEWAY_LOG_FORMAT")
        .map(|value| value.eq_ignore_ascii_case("json"))
        .unwrap_or(false);

    let fmt_layer: Box<dyn tracing_subscriber::Layer<_> + Send + Sync> = if json {
        Box::new(tracing_subscriber::fmt::layer().json())
    } else {
        Box::new(tracing_subscriber::fmt::layer())
    };

    let log_layer = logger
        .as_ref()
        .map(opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new);
    let trace_layer =
        tracer.map(|tracer| tracing_opentelemetry::layer().with_tracer(tracer.clone()));

    let subscriber = tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .with(log_layer)
        .with(trace_layer);

    // En tests el subscriber global puede estar ya instalado: no es un error.
    if let Err(err) = tracing::subscriber::set_global_default(subscriber) {
        tracing::debug!(error = %err, "subscriber de tracing ya instalado");
    }
    Ok(())
}

/// Compone la URL de una señal OTLP: `{endpoint}/api/{organization}/v1/{signal}`.
fn signal_url(otlp: &Otlp, signal: &str) -> String {
    format!(
        "{}/api/{}/v1/{signal}",
        otlp.endpoint.trim_end_matches('/'),
        otlp.organization
    )
}

/// Cabeceras adicionales de la exportación: `Authorization: Basic <token>`
/// cuando `token_env` está definido y la variable existe.
fn headers(otlp: &Otlp) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    if let Some(var) = &otlp.token_env
        && let Ok(token) = std::env::var(var)
        && !token.is_empty()
    {
        headers.insert("authorization".to_owned(), format!("Basic {token}"));
    }
    headers
}

/// Intervalo de exportación: `interval_ms` si está definido, o el por defecto.
fn interval(interval_ms: Option<u64>) -> Duration {
    Duration::from_millis(interval_ms.unwrap_or(DEFAULT_OTLP_INTERVAL_MS))
}

/// Timeout de exportación: `timeout_ms` si está definido, o el por defecto.
fn timeout(timeout_ms: Option<u64>) -> Duration {
    Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_OTLP_TIMEOUT_MS))
}

fn flush_metrics(provider: &SdkMeterProvider) {
    if let Err(err) = provider.force_flush() {
        log_flush_error("metrics", "flush", &err);
    }
    if let Err(err) = provider.shutdown() {
        log_flush_error("metrics", "shutdown", &err);
    }
}

fn flush_logs(provider: &SdkLoggerProvider) {
    if let Err(err) = provider.force_flush() {
        log_flush_error("logs", "flush", &err);
    }
    if let Err(err) = provider.shutdown() {
        log_flush_error("logs", "shutdown", &err);
    }
}

fn flush_traces(provider: &SdkTracerProvider) {
    if let Err(err) = provider.force_flush() {
        log_flush_error("traces", "flush", &err);
    }
    if let Err(err) = provider.shutdown() {
        log_flush_error("traces", "shutdown", &err);
    }
}

/// Registra el fallo de un flush/shutdown salvo cuando el provider ya estaba
/// cerrado: en ese caso (`AlreadyShutdown`) el resultado es el esperado —dos
/// guards pueden compartir los mismos providers— y no merece un `warn`.
fn log_flush_error(signal: &str, action: &str, err: &opentelemetry_sdk::error::OTelSdkError) {
    if matches!(err, opentelemetry_sdk::error::OTelSdkError::AlreadyShutdown) {
        tracing::debug!(
            signal,
            action,
            error = %err,
            "flush de telemetría repetido sobre providers ya cerrados"
        );
        return;
    }
    tracing::warn!(signal, action, error = %err, "flush de telemetría falló");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Protocol, Signals};

    fn otlp(endpoint: &str, token_env: Option<&str>, timeout_ms: Option<u64>) -> Otlp {
        Otlp {
            endpoint: endpoint.to_owned(),
            organization: "default".to_owned(),
            protocol: Protocol::Http,
            token_env: token_env.map(str::to_owned),
            service_name: None,
            interval_ms: None,
            timeout_ms,
            signals: Signals {
                metrics: true,
                logs: false,
                traces: false,
            },
        }
    }

    #[test]
    fn signal_url_composes_openobserve_path() {
        let otlp = otlp("http://localhost:5080", None, None);
        assert_eq!(
            signal_url(&otlp, "metrics"),
            "http://localhost:5080/api/default/v1/metrics"
        );
        assert_eq!(
            signal_url(&otlp, "logs"),
            "http://localhost:5080/api/default/v1/logs"
        );
        assert_eq!(
            signal_url(&otlp, "traces"),
            "http://localhost:5080/api/default/v1/traces"
        );
    }

    #[test]
    fn signal_url_trims_trailing_slash() {
        let otlp = otlp("http://localhost:5080/", None, None);
        assert_eq!(
            signal_url(&otlp, "metrics"),
            "http://localhost:5080/api/default/v1/metrics"
        );
    }

    #[test]
    fn headers_without_token_env_are_empty() {
        let otlp = otlp("http://localhost:5080", None, None);
        assert!(headers(&otlp).is_empty());
    }

    #[test]
    fn headers_with_token_env_use_basic_scheme() {
        let var = "GATEWAY_TEST_OTLP_AUTH_TOKEN";
        // SAFETY: nombre de variable único de test; el valor no es un secreto.
        unsafe { std::env::set_var(var, "abc123") };
        let otlp = otlp("http://localhost:5080", Some(var), None);
        let headers = headers(&otlp);
        unsafe { std::env::remove_var(var) };

        assert_eq!(
            headers.get("authorization").map(String::as_str),
            Some("Basic abc123")
        );
    }

    #[test]
    fn headers_ignore_missing_env_var() {
        let otlp = otlp(
            "http://localhost:5080",
            Some("GATEWAY_TEST_NO_SUCH_TOKEN"),
            None,
        );
        assert!(headers(&otlp).is_empty());
    }

    #[test]
    fn interval_defaults_to_15_seconds() {
        assert_eq!(interval(None), Duration::from_millis(15_000));
        assert_eq!(interval(Some(1_000)), Duration::from_millis(1_000));
    }

    #[test]
    fn timeout_defaults_to_10_seconds() {
        assert_eq!(timeout(None), Duration::from_millis(10_000));
        assert_eq!(timeout(Some(500)), Duration::from_millis(500));
    }

    #[test]
    fn signals_any_requires_one_enabled() {
        assert!(!Signals::default().any());
        assert!(
            Signals {
                metrics: false,
                logs: true,
                traces: false,
            }
            .any()
        );
    }
}
