# Exportación OTLP a OpenObserve

## Qué es y por qué

**OpenObserve** es un backend de observabilidad *OTLP-native*: acepta métricas,
logs y trazas por HTTP/OTLP. Este módulo añade al gateway la capacidad de
**exportar** esas tres señales a un colector OTLP (típicamente OpenObserve) para
tener una tubería única de telemetría, sin depender de Prometheus para el
almacenamiento.

La feature es **opt-in**: sin el bloque `observability.otlp` en el YAML el
gateway se comporta **exactamente** como antes. El endpoint `GET /metrics` en
formato Prometheus sigue existiendo y no requiere OpenObserve.

- `/metrics` (pull, Prometheus): **siempre** activo, lo sirve el
  `PrometheusExporter` de OTel sobre el registro global.
- OTLP (push, OpenObserve): solo si `observability.otlp` está presente y la
  señal correspondiente está a `true` en `signals`.

## Ficheros implicados

- `src/config.rs` — structs `Observability`, `Otlp`, `Protocol` y `Signals`, más
  la validación `validate_otlp` (endpoint y organización no vacíos,
  `interval_ms`/`timeout_ms` > 0 si están presentes, al menos una señal activa,
  y `token_env` resolvible).
- `src/telemetry.rs` — `TelemetryGuard`, `init`, `flush`, `planned_exporters`,
  la composición de URLs (`signal_url`), las cabeceras (`headers`), la
  construcción del `MeterProvider` (`PrometheusExporter` + `PeriodicReader`) y
  los providers de logs/trazas, además del subscriber de `tracing`.
- `src/metrics.rs` — instrumentos OTel (`Meter`/`Counter`/`Gauge`/`Histogram`)
  como **fuente única**; `render()` lee del mismo registro que alimenta `/metrics`.
- `src/main.rs` — llama a `telemetry::init(&config)` **antes** de construir
  `AppState` y a `telemetry.flush()` en el cierre ordenado.
- `src/proxy.rs` — propagación W3C: `link_remote_parent` (extrae `traceparent`
  entrante) e `inject_traceparent` (inyecta `traceparent` saliente al backend).
- `tests/telemetry.rs`, `tests/telemetry_disabled.rs` — receptor OTLP mock (axum)
  que captura y decodifica las exportaciones; backend mock que captura el
  `traceparent` recibido.

## Cómo funciona

### Fuente única OTel

`metrics.rs` ya no usa atómicos pre-registrados: crea **instrumentos OTel** una
vez a partir de la configuración (cardinalidad acotada por el YAML, igual que
antes). Un único `MeterProvider` global lleva **dos readers**:

- `PrometheusExporter` — sirve `GET /metrics` con el contrato de siempre
  (`text/plain; version=0.0.4`, series `llm_gateway_*`). **Siempre** activo, se
  construye con `without_target_info()` y `scope_info_enabled(false)` para no
  añadir series ajenas al contrato.
- `PeriodicReader` con exportador OTLP — **solo** si `signals.metrics = true`,
  con el intervalo de `interval_ms`.

Los instrumentos *counter* se nombran sin `_total` (`llm_gateway_requests`) para
que el texto Prometheus conserve la serie `llm_gateway_requests_total` (OTel
añade el sufijo). Los histogramas exponen `_bucket`/`_sum`/`_count`, igual que
hoy. La fachada pública `record_*` de `Metrics` se conserva.

### Providers por señal

- `MeterProvider`: siempre (para `/metrics`); reader OTLP condicional.
- `LoggerProvider` (`opentelemetry-appender-tracing`): layer de `tracing` solo si
  `signals.logs = true`; exportador **por lotes**.
- `TracerProvider` (`tracing-opentelemetry`): layer de `tracing` solo si
  `signals.traces = true`; exportador por lotes. Cuando hay trazas se instala
  además el **propagador W3C** (`TraceContextPropagator`).

Cada señal desactivada o ausente **no se inicializa**: cero exportadores, cero
cambio de comportamiento.

### Endpoints y propagación

La URL de cada señal se compone como
`{endpoint}/api/{organization}/v1/{metrics|logs|traces}` (con *trim* de la barra
final del `endpoint`), que es la ruta que espera el colector de OpenObserve.

Con trazas activas, el gateway:

- **extrae** el `traceparent` entrante y lo usa como padre del span de servidor
  (si es válido; si no, la traza arranca de cero);
- **inyecta** el `traceparent` del span en la petición al backend, de modo que la
  traza distribuida continúa en el upstream.

### Cierre ordenado

`TelemetryGuard` (RAII) hace `force_flush` + `shutdown` de los providers. En
`main.rs`, tras el *graceful shutdown*, se **abortan primero los handles de
warmup** —para que no registren señales contra instrumentos que se van a
cerrar— y después se llama a `telemetry.flush()`. El `Drop` del guard repite el flush,
así que las señales pendientes no se pierden aunque `axum::serve` o el `bind`
fallen antes de llegar al apagado ordenado; `flush()` es idempotente.

## Configuración

```yaml
observability:
  otlp:
    endpoint: "http://localhost:5080"     # base, sin /api/<org>
    organization: "default"
    protocol: http                        # único soportado hoy (grpc se rechaza)
    token_env: "OPENOBSERVE_TOKEN"        # nombre de la variable, nunca el token
    service_name: "llm-gateway"           # recurso OTel (opcional)
    interval_ms: 15000                    # default 15000
    timeout_ms: 10000                     # default 10000
    signals:
      metrics: true
      logs: true
      traces: true
```

Structs Rust equivalentes:

```rust
pub struct Observability {
    pub otlp: Option<Otlp>,
}

pub struct Otlp {
    pub endpoint: String,             // base, sin /api/<organización>
    pub organization: String,
    pub protocol: Protocol,           // solo Http soportado (default Http)
    pub token_env: Option<String>,
    pub service_name: Option<String>,
    pub interval_ms: Option<u64>,     // default 15000
    pub timeout_ms: Option<u64>,      // default 10000
    pub signals: Signals,
}

pub enum Protocol { Http, Grpc }      // Grpc se parsea pero se rechaza al validar

pub struct Signals {
    pub metrics: bool,                // ausente → false
    pub logs: bool,
    pub traces: bool,
}
```

Defaults y reglas:

- `protocol`: solo `http` (OTLP/HTTP con protobuf) está **soportado**. `grpc` se
  parsea para poder dar un error de configuración explícito
  (`'grpc' aún no está soportado; usa 'http'`) en vez de ignorarse en silencio.
- `interval_ms`: default `15000` (15 s). Si está presente debe ser **> 0**;
  aplica a la exportación periódica de métricas y logs.
- `timeout_ms`: default `10000` (10 s). Si está presente debe ser **> 0**.
- `signals`: cada campo ausente queda a `false`; **al menos una** señal debe estar
  activa o la carga de la configuración falla.
- `service_name`: default `llm-gateway`, reportado como recurso OTel.
- `endpoint` no puede estar vacío y debe empezar por `http://` o `https://`.
- `organization` no puede estar vacía ni contener espacios o `/` (se interpola en
  la ruta `/api/{organization}/v1/{signal}`).
- `token_env` es **opcional**. Si está definido y la variable de entorno **no
  existe o está vacía**, `config::load` falla en el arranque (mejor fallar pronto
  que exportar sin autenticación). El token se envía como cabecera
  `Authorization: Basic <token>`; **nunca** se escribe en el YAML.

### Cómo habilitar

1. Añade el bloque `observability.otlp` a tu `gateway.yaml` (ver arriba).
2. Exporta el token si usas `token_env`:

   ```bash
   export OPENOBSERVE_TOKEN="<token-de-ingesta>"
   GATEWAY_CONFIG=./gateway.yaml llm-gateway
   ```

3. Genera algo de tráfico para producir señales:

   ```bash
   curl -s localhost:8080/v1/chat/completions \
     -H 'content-type: application/json' \
     -d '{"model":"mech-writer","messages":[{"role":"user","content":"hola"}]}'
   ```

### Verificar en OpenObserve

- **Métricas**: OpenObserve las recibe como métricas OTLP; aparecen con el
  prefijo del *scope* del gateway (`llm-gateway`). Busca series como
  `llm_gateway_requests` / `llm_gateway_backend_attempts`.
- **Logs**: los eventos de `tracing` llegan al *stream* de logs de la
  organización indicada. Se exportan los campos **del evento**, no los del span
  que lo cubre: los eventos resumen de petición (`petición completada`,
  `petición fallida`) llevan `request_id`, `route`, `streaming`, `outcome`,
  `attempts` y `latency_ms`; los de rechazo previo (`modelo desconocido`,
  `petición sin 'model' válido`) llevan `request_id` y, si aplica, `model`. Los
  mensajes `debug` de intento individual llevan `backend`, `result` y
  `latency_ms`.
- **Trazas**: consulta por `trace_id`; la traza *raíz* es el span de servidor del
  gateway y, si el cliente envió `traceparent`, cuelga de él. Los spans de cliente
  hacia el backend llevan el contexto inyectado.

Sin bloque `observability`, ni Prometheus ni OpenObserve cambian: `/metrics`
sigue rindiendo las mismas series y no se envía nada por OTLP.

## Decisiones y limitaciones

- **Fuente única OTel**: atómicos → instrumentos OTel, con dos readers sobre un
  solo `MeterProvider`. Evita doble registro y deriva entre `/metrics` y OTLP;
  la alternativa (atómicos + conversión propia a OTLP) duplicaba la fuente.
  Ese `MeterProvider` se **memoiza** (`OnceLock`): el `PrometheusExporter` solo
  puede registrar su collector una vez, así que `init` reutiliza el provider ya
  instalado en vez de construir otro (antes, un `init` tras
  `ensure_meter_provider` fallaba con `Duplicate metrics collector registration`).
  Consecuencia: si un consumidor de la librería crea `Metrics` **antes** de
  `init`, el provider reutilizado no tiene reader OTLP y las métricas no se
  exportan por OTLP (se registra un `warn`); logs y trazas sí se instalan.
- **Opt-in estricto**: sin bloque, comportamiento idéntico al anterior; el
  `PrometheusExporter` se instala igualmente porque es la fuente de `/metrics`.
- **Credenciales por entorno**: el token vive en una variable de entorno, no en
  el YAML; el arranque falla si `token_env` no resuelve.
- **Cliente OTLP bloqueante → runtime multi-hilo**: `opentelemetry-otlp` usa la
  feature `reqwest-blocking-client`, que bloquea el hilo durante la exportación.
  Por eso los tests que ejercitan el camino OTLP usan
  `#[tokio::test(flavor = "multi_thread", worker_threads = 2)]`: con el runtime
  mono-hilo por defecto, bloquear dentro de una tarea async provoca un *panic*.
- **`PeriodicReader` 0.33 sin `with_timeout`**: la versión de `opentelemetry_sdk`
  usada (0.33) no expone `with_timeout()` en el `PeriodicReader`; el `timeout_ms`
  se aplica al **exportador** (`MetricExporter::builder().with_timeout(...)`) y el
  reader solo acepta `with_interval()`. En logs y trazas el `timeout_ms` va
  directamente al exportador por lotes.
- **Estado global, una vez por proceso**: los providers OTel globales y el
  subscriber de `tracing` solo se pueden instalar una vez por proceso; el
  escenario «camino feliz» vive en un único test de `tests/telemetry.rs` y el de
  «señales desactivadas» en `tests/telemetry_disabled.rs` (binario/proceso
  aparte).
- **Dependencias**: `opentelemetry`/`opentelemetry_sdk`/`opentelemetry-otlp`/
  `opentelemetry-prometheus`/`opentelemetry-appender-tracing`/`opentelemetry-http`
  en **0.33**, `tracing-opentelemetry` 0.34 y `prometheus` 0.14; `reqwest` 0.13
  (la misma que ya usaba el proxy, con `rustls`). En `dev-dependencies`,
  `opentelemetry-proto` 0.33 y `prost` 0.14 se usan para **decodificar** las
  exportaciones en los tests.

## Follow-ups y limitaciones

- **Sin verificación contra OpenObserve real**: los tests usan un receptor mock
  que decodifica protobuf OTLP; queda validar contra una instancia real de
  OpenObserve (token de ingesta, *streams* por defecto, retención).
- **`protocol: grpc` sin implementar**: hoy el transporte es OTLP/HTTP y `grpc`
  se **rechaza** en la validación de configuración. Implementarlo obligaría a
  arrastrar `tonic` y no está probado extremo a extremo.
- **Sin métricas de negocio**: el gateway no interpreta el cuerpo, así que no
  exporta tokens ni coste (fuera de alcance por diseño).
- **Sin sampling, dashboards ni alertas**: esta feature solo transporta señales.
- **Considerar `with_timeout` a nivel de reader** cuando `opentelemetry_sdk`
  ofrezca un `PeriodicReader` configurable en tiempo de exportación.
