//! Test de integración del camino OTLP del gateway.
//!
//! Un receptor mock (axum) escucha en `/api/{org}/v1/{metrics,logs,traces}` y
//! captura cabeceras y cuerpo de cada exportación; un backend mock captura las
//! cabeceras que le envía el gateway (para comprobar la propagación de
//! `traceparent`).
//!
//! # Nota sobre el estado global
//!
//! Los providers OTel globales y el subscriber de `tracing` solo pueden
//! instalarse **una vez por proceso**. Por eso el camino feliz (métricas, logs
//! y trazas) vive en un único `#[tokio::test]`; los casos de "señal
//! desactivada" viven en `tests/telemetry_disabled.rs` (proceso aparte) o, como
//! el plan de exportadores, no llaman a `telemetry::init` en absoluto.

mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use llm_gateway_rs::config::GatewayConfig;
use llm_gateway_rs::proxy::{AppState, router};
use llm_gateway_rs::telemetry;
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value;
use opentelemetry_proto::tonic::metrics::v1::{Metric, metric};
use opentelemetry_proto::tonic::trace::v1::{Span, span};
use prost::Message as _;
use serde_json::json;
use tokio::net::TcpListener;

/// Una petición capturada por el receptor OTLP mock.
#[derive(Clone, Debug, Default)]
struct Captured {
    authorization: Option<String>,
    content_type: Option<String>,
    body: Vec<u8>,
}

/// Capturas del receptor, por señal (`metrics`/`logs`/`traces`).
type CapturedMap = Arc<Mutex<BTreeMap<String, Vec<Captured>>>>;

/// Estado del receptor OTLP mock.
#[derive(Clone)]
struct Receiver {
    captured: CapturedMap,
}

impl Receiver {
    /// Guarda una exportación para la `signal` dada.
    fn record(&self, signal: &str, headers: &HeaderMap, body: &Bytes) {
        let entry = Captured {
            authorization: headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned),
            content_type: headers
                .get("content-type")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned),
            body: body.to_vec(),
        };
        self.captured
            .lock()
            .expect("lock de capturas")
            .entry(signal.to_owned())
            .or_default()
            .push(entry);
    }
}

/// Levanta el receptor OTLP mock y devuelve su URL base y las capturas.
async fn spawn_otlp_receiver() -> (String, CapturedMap) {
    async fn metrics(
        State(receiver): State<Receiver>,
        Path(_org): Path<String>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        receiver.record("metrics", &headers, &body);
        StatusCode::OK.into_response()
    }
    async fn logs(
        State(receiver): State<Receiver>,
        Path(_org): Path<String>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        receiver.record("logs", &headers, &body);
        StatusCode::OK.into_response()
    }
    async fn traces(
        State(receiver): State<Receiver>,
        Path(_org): Path<String>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        receiver.record("traces", &headers, &body);
        StatusCode::OK.into_response()
    }

    let captured: CapturedMap = Arc::new(Mutex::new(BTreeMap::new()));
    let receiver = Receiver {
        captured: Arc::clone(&captured),
    };
    let app = Router::new()
        .route("/api/{org}/v1/metrics", post(metrics))
        .route("/api/{org}/v1/logs", post(logs))
        .route("/api/{org}/v1/traces", post(traces))
        .with_state(receiver);

    (serve(app).await, captured)
}

/// Petición recibida por el backend mock.
#[derive(Clone, Debug, Default)]
struct BackendHit {
    traceparent: Option<String>,
}

/// Levanta un backend OpenAI-compatible que captura las cabeceras de cada POST.
async fn spawn_capturing_backend() -> (String, Arc<Mutex<Vec<BackendHit>>>) {
    async fn chat(State(hits): State<Arc<Mutex<Vec<BackendHit>>>>, headers: HeaderMap) -> Response {
        hits.lock().expect("lock de hits").push(BackendHit {
            traceparent: headers
                .get("traceparent")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned),
        });
        axum::Json(json!({
            "id": "chatcmpl-telemetry",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hola"},
                "finish_reason": "stop"
            }]
        }))
        .into_response()
    }

    let hits = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .route("/v1/chat/completions", post(chat))
        .with_state(Arc::clone(&hits));

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve backend mock");
    });
    (format!("http://{addr}/v1"), hits)
}

/// Sirve `app` en un puerto efímero y devuelve su URL base (sin sufijo).
async fn serve(app: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve mock");
    });
    format!("http://{addr}")
}

/// Decodifica todas las métricas recibidas, de todas las exportaciones.
fn all_metrics(captured: &BTreeMap<String, Vec<Captured>>) -> Vec<Metric> {
    let mut metrics = Vec::new();
    for entry in captured.get("metrics").into_iter().flatten() {
        let request = ExportMetricsServiceRequest::decode(entry.body.as_slice())
            .expect("las métricas OTLP deben ser protobuf válido");
        for resource in request.resource_metrics {
            for scope in resource.scope_metrics {
                metrics.extend(scope.metrics);
            }
        }
    }
    metrics
}

/// Decodifica todos los spans recibidos, de todas las exportaciones.
fn all_spans(captured: &BTreeMap<String, Vec<Captured>>) -> Vec<Span> {
    let mut spans = Vec::new();
    for entry in captured.get("traces").into_iter().flatten() {
        let request = ExportTraceServiceRequest::decode(entry.body.as_slice())
            .expect("las trazas OTLP deben ser protobuf válido");
        for resource in request.resource_spans {
            for scope in resource.scope_spans {
                spans.extend(scope.spans);
            }
        }
    }
    spans
}

/// Decodifica todos los logs recibidos, de todas las exportaciones.
fn all_logs(
    captured: &BTreeMap<String, Vec<Captured>>,
) -> Vec<opentelemetry_proto::tonic::logs::v1::LogRecord> {
    let mut logs = Vec::new();
    for entry in captured.get("logs").into_iter().flatten() {
        let request = ExportLogsServiceRequest::decode(entry.body.as_slice())
            .expect("los logs OTLP deben ser protobuf válido");
        for resource in request.resource_logs {
            for scope in resource.scope_logs {
                logs.extend(scope.log_records);
            }
        }
    }
    logs
}

/// Valor de cadena de un atributo OTLP.
fn attr_string(
    attributes: &[opentelemetry_proto::tonic::common::v1::KeyValue],
    key: &str,
) -> Option<String> {
    attributes
        .iter()
        .find(|kv| kv.key == key)
        .and_then(|kv| kv.value.as_ref())
        .and_then(|value| match value.value.as_ref() {
            Some(any_value::Value::StringValue(text)) => Some(text.clone()),
            _ => None,
        })
}

/// Valor booleano de un atributo OTLP.
fn attr_bool(
    attributes: &[opentelemetry_proto::tonic::common::v1::KeyValue],
    key: &str,
) -> Option<bool> {
    attributes
        .iter()
        .find(|kv| kv.key == key)
        .and_then(|kv| kv.value.as_ref())
        .and_then(|value| match value.value.as_ref() {
            Some(any_value::Value::BoolValue(flag)) => Some(*flag),
            _ => None,
        })
}

/// Valor entero de un atributo OTLP.
fn attr_int(
    attributes: &[opentelemetry_proto::tonic::common::v1::KeyValue],
    key: &str,
) -> Option<i64> {
    attributes
        .iter()
        .find(|kv| kv.key == key)
        .and_then(|kv| kv.value.as_ref())
        .and_then(|value| match value.value.as_ref() {
            Some(any_value::Value::IntValue(number)) => Some(*number),
            _ => None,
        })
}

/// Cuerpo de un log OTLP como texto, si es una cadena.
fn body_text(record: &opentelemetry_proto::tonic::logs::v1::LogRecord) -> Option<&str> {
    record
        .body
        .as_ref()
        .and_then(|value| match value.value.as_ref() {
            Some(any_value::Value::StringValue(text)) => Some(text.as_str()),
            _ => None,
        })
}

/// Valor entero de los data points de un `Sum` monotónico (counter).
fn sum_values(
    metric: &Metric,
) -> Vec<(Vec<opentelemetry_proto::tonic::common::v1::KeyValue>, i64)> {
    let Some(metric::Data::Sum(sum)) = metric.data.as_ref() else {
        return Vec::new();
    };
    sum.data_points
        .iter()
        .filter_map(|point| match point.value.as_ref() {
            Some(opentelemetry_proto::tonic::metrics::v1::number_data_point::Value::AsInt(
                value,
            )) => Some((point.attributes.clone(), *value)),
            _ => None,
        })
        .collect()
}

/// Cadena hexadecimal de un `trace_id`/`span_id` OTLP (bytes).
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Camino feliz completo: métricas + logs + trazas por OTLP con token.
///
/// Es el **único** test del proceso que llama a `telemetry::init` (los providers
/// globales y el subscriber solo se instalan una vez).
///
/// Runtime **multi-hilo**: los exportadores OTLP son bloqueantes
/// (`reqwest-blocking-client`) y `TelemetryGuard::flush` espera de forma
/// sincrónica; con el runtime de un solo hilo el receptor mock no podría
/// responder y toda exportación terminaría en timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn otlp_happy_path_exports_metrics_logs_and_traces() {
    let (receiver_url, captured) = spawn_otlp_receiver().await;
    let (backend_url, hits) = spawn_capturing_backend().await;

    let token_var = format!("GATEWAY_TEST_OTLP_TOKEN_{}", std::process::id());
    let token = "token-de-prueba";
    // SAFETY: nombre de variable único para este test; el binario es un proceso
    // aparte y ningún otro test toca esta variable.
    unsafe { std::env::set_var(&token_var, token) };

    let yaml = format!(
        "route:\n  - name: telemetry-route\n    backend:\n      - base_url: \"{backend_url}\"\n        model: \"m\"\n\
         observability:\n  otlp:\n    endpoint: \"{receiver_url}\"\n    organization: \"default\"\n    protocol: http\n\
         \x20   token_env: \"{token_var}\"\n    service_name: \"llm-gateway-test\"\n    interval_ms: 200\n    signals:\n\
         \x20     metrics: true\n      logs: true\n      traces: true\n"
    );
    let config: GatewayConfig = serde_saphyr::from_str(&yaml).expect("config de test válida");

    let guard = telemetry::init(&config).expect("init de telemetría");
    assert!(
        guard.exports_otlp(),
        "el guard debe declarar exportación OTLP"
    );

    let state = AppState::new(config).expect("construir AppState");
    let gateway = serve(router(state)).await;

    // Petición con `traceparent` entrante: el gateway debe continuar la traza.
    let trace_id = "0af7651916cd43dd8448eb211c80319c";
    let parent_span_id = "b7ad6b7169203331";
    let response = reqwest::Client::new()
        .post(format!("{gateway}/v1/chat/completions"))
        .header("traceparent", format!("00-{trace_id}-{parent_span_id}-01"))
        .json(&json!({"model": "telemetry-route", "messages": []}))
        .send()
        .await
        .expect("request al gateway");
    assert_eq!(response.status(), StatusCode::OK);

    // Segunda petición con modelo desconocido: genera un log `warn` con campos.
    let unknown = "modelo-desconocido-telemetry";
    let response = reqwest::Client::new()
        .post(format!("{gateway}/v1/chat/completions"))
        .json(&json!({"model": unknown, "messages": []}))
        .send()
        .await
        .expect("request al gateway");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // Vuelca métricas, logs y trazas pendientes antes de inspeccionar.
    guard.flush();
    unsafe { std::env::remove_var(&token_var) };

    let captured = captured.lock().expect("lock de capturas").clone();

    // (a) El token va como `Authorization: Basic <token>` y el cuerpo es protobuf.
    for signal in ["metrics", "logs", "traces"] {
        let entries = captured
            .get(signal)
            .unwrap_or_else(|| panic!("no llegó ninguna exportación de {signal}"));
        let entry = entries.first().expect("al menos una exportación");
        assert_eq!(
            entry.authorization.as_deref(),
            Some(format!("Basic {token}").as_str()),
            "{signal}: cabecera Authorization inesperada"
        );
        assert!(
            entry
                .content_type
                .as_deref()
                .is_some_and(|value| value.contains("protobuf")),
            "{signal}: content-type OTLP inesperado: {:?}",
            entry.content_type
        );
    }

    // (b) Métricas: data points con los nombres y valores esperados.
    let metrics = all_metrics(&captured);
    let requests = metrics
        .iter()
        .find(|metric| metric.name == "llm_gateway_requests")
        .expect("falta la métrica llm_gateway_requests");
    assert!(
        sum_values(requests).iter().any(|(attributes, value)| {
            attr_string(attributes, "route").as_deref() == Some("telemetry-route")
                && attr_string(attributes, "streaming").as_deref() == Some("false")
                && attr_string(attributes, "outcome").as_deref() == Some("success")
                && *value == 1
        }),
        "no hay un data point de llm_gateway_requests con route/streaming/outcome y valor 1"
    );

    let attempts = metrics
        .iter()
        .find(|metric| metric.name == "llm_gateway_backend_attempts")
        .expect("falta la métrica llm_gateway_backend_attempts");
    assert!(
        sum_values(attempts).iter().any(|(attributes, value)| {
            attr_string(attributes, "result").as_deref() == Some("success") && *value == 1
        }),
        "no hay un data point de llm_gateway_backend_attempts con result=success y valor 1"
    );

    let durations = metrics
        .iter()
        .find(|metric| metric.name == "llm_gateway_request_duration_seconds")
        .expect("falta el histograma llm_gateway_request_duration_seconds");
    assert!(
        matches!(durations.data.as_ref(), Some(metric::Data::Histogram(_))),
        "llm_gateway_request_duration_seconds debe ser un histograma"
    );

    // (c) Logs: el evento resumen de la petición conserva sus campos
    // estructurados (`request_id`, `route`, `streaming`, `outcome`, `attempts`,
    // `latency_ms`). El bridge de `opentelemetry-appender-tracing` exporta los
    // campos del **evento**, no los del span: por eso deben ir en el evento.
    let logs = all_logs(&captured);
    let log = logs
        .iter()
        .find(|record| body_text(record).is_some_and(|text| text.contains("petición completada")))
        .unwrap_or_else(|| {
            panic!("no llegó el log resumen 'petición completada'; recibidos: {logs:?}")
        });

    assert_eq!(
        attr_string(&log.attributes, "route").as_deref(),
        Some("telemetry-route"),
        "el log resumen debe conservar `route`, atributos: {:?}",
        log.attributes
    );
    assert_eq!(
        attr_bool(&log.attributes, "streaming"),
        Some(false),
        "el log resumen debe conservar `streaming`, atributos: {:?}",
        log.attributes
    );
    assert!(
        attr_string(&log.attributes, "request_id").is_some(),
        "el log resumen debe conservar `request_id`, atributos: {:?}",
        log.attributes
    );
    assert_eq!(
        attr_string(&log.attributes, "outcome").as_deref(),
        Some("success"),
        "el log resumen debe conservar `outcome`, atributos: {:?}",
        log.attributes
    );
    assert_eq!(
        attr_int(&log.attributes, "attempts"),
        Some(1),
        "el log resumen debe conservar `attempts`, atributos: {:?}",
        log.attributes
    );
    assert!(
        attr_int(&log.attributes, "latency_ms").is_some(),
        "el log resumen debe conservar `latency_ms`, atributos: {:?}",
        log.attributes
    );
    assert!(
        !log.trace_id.is_empty(),
        "el log de la petición debe llevar trace_id"
    );

    // Los rechazos previos al enrutado también exportan sus campos.
    let rejection = logs
        .iter()
        .find(|record| {
            body_text(record).is_some_and(|text| text.contains("modelo desconocido"))
                && attr_string(&record.attributes, "model").as_deref() == Some(unknown)
        })
        .unwrap_or_else(|| {
            panic!(
                "no llegó un log con el mensaje 'modelo desconocido' y model={unknown}; recibidos: {logs:?}"
            )
        });
    assert!(
        attr_string(&rejection.attributes, "request_id").is_some(),
        "el warn de modelo desconocido debe conservar `request_id`"
    );

    // (d) Trazas: span de servidor y de cliente, con la traza distribuida.
    let spans = all_spans(&captured);
    let server = spans
        .iter()
        .find(|span| span.name == "request")
        .expect("falta el span de servidor 'request'");
    let client = spans
        .iter()
        .find(|span| span.name == "backend_request")
        .expect("falta el span de cliente 'backend_request'");

    assert_eq!(server.kind, span::SpanKind::Server as i32, "kind server");
    assert_eq!(client.kind, span::SpanKind::Client as i32, "kind client");
    assert_eq!(
        hex(&server.trace_id),
        trace_id,
        "el span de servidor debe continuar la traza entrante"
    );
    assert_eq!(
        hex(&server.parent_span_id),
        parent_span_id,
        "el span de servidor debe colgar del `traceparent` entrante"
    );
    assert_eq!(
        client.trace_id, server.trace_id,
        "el span de cliente debe compartir la traza del servidor"
    );
    assert_eq!(
        client.parent_span_id, server.span_id,
        "el span de cliente debe colgar del span de servidor"
    );

    // El `traceparent` inyectado llegó al backend con la misma traza.
    let hits = hits.lock().expect("lock de hits").clone();
    let traceparent = hits
        .first()
        .and_then(|hit| hit.traceparent.clone())
        .expect("el backend debe recibir `traceparent`");
    assert!(
        traceparent.contains(trace_id),
        "el `traceparent` inyectado debe llevar la traza entrante: {traceparent}"
    );
}

/// Los providers planificados siguen a `signals` (función pura, sin estado global).
#[test]
fn planned_exporters_follow_signals() {
    let config = |metrics: bool, logs: bool, traces: bool| -> GatewayConfig {
        let yaml = format!(
            "route:\n  - name: r\n    backend:\n      - base_url: \"http://127.0.0.1:1/v1\"\n        model: \"m\"\n\
             observability:\n  otlp:\n    endpoint: \"http://localhost:5080\"\n    organization: \"default\"\n\
             \x20   signals:\n      metrics: {metrics}\n      logs: {logs}\n      traces: {traces}\n"
        );
        serde_saphyr::from_str(&yaml).expect("config de test válida")
    };

    assert!(telemetry::planned_exporters(&config(true, false, false)).contains(&"metrics"));
    assert_eq!(
        telemetry::planned_exporters(&config(false, false, true)),
        vec!["traces"]
    );
    assert_eq!(
        telemetry::planned_exporters(&config(false, false, false)).len(),
        0
    );
}

/// Sin bloque `observability` no se planifica ninguna exportación OTLP
/// (función pura: no instala providers).
#[test]
fn no_observability_block_plans_no_exporters() {
    let yaml = "route:\n  - name: r\n    backend:\n      - base_url: \"http://127.0.0.1:1/v1\"\n        model: \"m\"\n";
    let config: GatewayConfig = serde_saphyr::from_str(yaml).expect("config de test válida");

    assert!(telemetry::planned_exporters(&config).is_empty());
}

// Nota: el contrato de `/metrics` sin observabilidad se prueba en
// `tests/metrics.rs`. Aquí no puede convivir con el camino feliz porque el
// `shutdown` de providers del `TelemetryGuard` (que el camino feliz ejercita)
// vaciaría el registro Prometheus compartido para el resto de tests.
