# Ledger de observabilidad: eventos `catalog`, `selection` y `request`

## Qué es y por qué

El **ledger de observabilidad** (#39) convierte cada decisión del gateway en
un **evento estructurado** que viaja como log de `tracing` hacia OpenObserve
(por el puente OTLP de logs, `opentelemetry-appender-tracing`). Con los tres
eventos se puede reconstruir en SQL el histórico completo de precios del
catálogo, qué modelo eligió cada ruta y por qué, y qué costó cada petición —
algo que las métricas Prometheus (agregadas y sin histórico) no permiten.

La emisión es **opt-out**: `observability.events: false` en el YAML suprime
los tres eventos. Las métricas Prometheus y la exportación OTLP **no se
afectan**: el opt-out solo apaga el log, nunca los instrumentos.

- Los eventos se emiten en el hot path como `tracing::info!` y solo llegan a
  OpenObserve si `observability.otlp.signals.logs: true`. Sin exportación
  configurada, siguen visibles en stdout/JSON.
- Un fallo del emisor queda aislado (`catch_unwind`): nunca altera la
  respuesta del cliente ni el `finish_reason`.

## Configuración

```yaml
# gateway.yaml
observability:
  # Emisión de los tres eventos del ledger. Default true.
  # false suprime catalog, selection y request; métricas y OTLP siguen.
  events: false
  otlp:
    endpoint: "http://localhost:5080"
    organization: "default"
    signals:
      logs: true        # sin logs: true los eventos no llegan a OpenObserve
```

| Campo | Default | Efecto |
|---|---|---|
| `observability.events` | `true` | `false` desactiva la emisión de los tres eventos del ledger |
| `observability.otlp.signals.logs` | — | sin él, los eventos solo salen por stdout |

Ficheros implicados:

- `src/config.rs` — campo `Observability.events` (default `serde true`) y
  resolver `GatewayConfig::ledger_events_enabled()`.
- `src/discovery.rs` — eventos `catalog` (`emit_catalog_events`) y `selection`
  (`emit_selection_events`); el flag vive en `CatalogCache` y `Registry`.
- `src/proxy.rs` — evento `request` (`emit_request_event`, tanto en la vía de
  respuesta completa como en `RequestSink::complete` de streaming).
- `src/metrics.rs` — wiring de métricas del ledger (Grupo 5), independiente
  del flag de eventos.
- `tests/observability_events_optout.rs` — integración del opt-out;
  `tests/config.rs` — parseo y defaults.

## Los tres eventos

Los tres comparten `event_type` (el discriminador de consulta) y `ts`
(milisegundos desde época, ms de UNIX).

### 1) `gateway_catalog` — `event_type: catalog`

Uno por modelo cuyo precio cambia o que aparece/desaparece del catálogo
(deduplicado en memoria contra el último snapshot observado). Se emite en cada
refresco del catálogo, incluido el refresco on-demand de la petición.

| Campo | Tipo | Significado |
|---|---|---|
| `event_type` | `catalog` | discriminador |
| `ts` | u64 ms | momento del refresco |
| `model_id` | string | id del modelo (ej. `acme/flash-a`) |
| `org` | string | organización (prefijo de `model_id`) |
| `pin` / `pout` | f64 | precio entrada/salida en USD/Mtok |
| `blend` | f64 | `0.75·pin + 0.25·pout` (USD/Mtok) |
| `context_length` | u64 | contexto del modelo |
| `change_kind` | enum | `new`, `price_up`, `price_down`, `removed` |
| `prev_pin` / `prev_pout` | f64? | solo si el modelo ya estaba y sigue en catálogo |

### 2) `gateway_selection` — `event_type: selection`

Uno por cambio del modelo elegido de un `(route, backend)` de pago. Un cambio
al **mismo** modelo no emite evento.

| Campo | Tipo | Significado |
|---|---|---|
| `event_type` | `selection` | discriminador |
| `ts` | u64 ms | momento del cambio |
| `route` | string | nombre de la ruta lógica |
| `backend` | string | `base_url` del backend de pago |
| `model_id` | string | modelo recién elegido |
| `reason` | enum | `initial`, `cheaper`, `failover`, `removed` |
| `pin` / `pout` | f64 | precio del nuevo modelo (USD/Mtok) |
| `prev_model_id` | string? | solo si había un modelo elegido anterior |

### 3) `gateway_requests` — `event_type: request`

Uno por petición atendida, con éxito o agotando la cadena de failover. En
streaming se emite al fin real del stream (chunk centinela), fuera del camino
de bytes.

| Campo | Tipo | Significado |
|---|---|---|
| `event_type` | `request` | discriminador |
| `ts` | u64 ms | fin de la petición |
| `request_id` | string | header del cliente (deduplicación/lookup) |
| `route` | string | nombre de la ruta lógica |
| `model_id` | string? | modelo usado; el último intento si todo falló |
| `streaming` | bool | `true` si la petición era SSE |
| `attempts` | u32 | intentos consumidos |
| `failover` | bool | `attempts > 1` |
| `outcome` | string | `success` o `all_failed` |
| `finish_reason` | string? | del upstream (`stop`, `tool_calls`, …) |
| `latency_ms` | u64 | latencia total |
| `tokens_in` / `tokens_out` | u64? | del `usage` del upstream |
| `pin` / `pout` | f64? | precios del modelo **en el momento de la petición** |
| `cost_usd` | f64? | `tokens_in·pin + tokens_out·pout`, en USD |

**Requisito de `usage`**: los tokens y el coste solo se registran si el
upstream devuelve `usage` (el gateway inyecta `stream_options.include_usage`
en streaming). Sin `usage`, los campos quedan **desconocidos** (`null` en el
evento; en Prometheus, el contador simplemente no suma). El gateway **nunca
inventa** un `0`: un `0` solo aparece con `usage` informado. Para tener datos
de coste completos, verifica que el proveedor upstream devuelve `usage` en sus
respuestas (OpenRouter lo hace; otro upstream de OpenAI-compatible puede no
hacerlo).

## Retención recomendada (se configura en OpenObserve)

La retención **no es configuración del gateway**: se define en OpenObserve
por *stream* (Settings → Streams → Retention, o en el *stream settings* de la
organización). El gateway solo decide qué se emite.

| Evento | Retención recomendada | Motivo |
|---|---|---|
| `catalog` | **1–2 años** | histórico de precios: la base para auditorías de coste y para justificar decisiones de ruteo a posteriori |
| `selection` | **1–2 años** | mismo criterio: pequeña en volumen y valiosa para reconstruir el comportamiento del router |
| `request` | **30–90 días** | alto volumen y datos personales del cliente; una ventana operativa de depuración y facturación reciente |

## Consultas SQL de ejemplo

Los eventos aterrizan en el *stream* de logs de la organización (por defecto
`default`; si tu ingesta usa la cabecera `stream-name`, sustitúyelo). Los
atributos del LogRecord OTLP quedan como campos del registro; ajusta el nombre
de la columna si tu versión los anida.

**Coste por modelo y día** (solo peticiones con coste conocido):

```sql
SELECT
  histogram(_timestamp, '1 day') AS día,
  model_id,
  SUM(cost_usd) AS coste_usd,
  COUNT(*) AS peticiones,
  SUM(tokens_in) AS tokens_entrada,
  SUM(tokens_out) AS tokens_salida
FROM "default"
WHERE event_type = 'request'
  AND cost_usd IS NOT NULL
GROUP BY día, model_id
ORDER BY día DESC, coste_usd DESC
```

*(En versiones antiguas de OpenObserve el intervalo de `histogram` va en
segundos: `histogram(_timestamp, 86400)`.)*

**Evolución de precio de una familia** (todos los modelos de una org):

```sql
SELECT
  histogram(_timestamp, '1 day') AS día,
  model_id,
  MIN(blend) AS blend_mínimo,
  MAX(blend) AS blend_máximo,
  MAX(ts) AS última_señal
FROM "default"
WHERE event_type = 'catalog'
  AND org = 'acme'
GROUP BY día, model_id
ORDER BY día DESC, model_id
```

Para el detalle paso a paso de una familia concreta, sin agregar:

```sql
SELECT ts, model_id, pin, pout, blend, change_kind, prev_pin, prev_pout
FROM "default"
WHERE event_type = 'catalog'
  AND model_id LIKE 'acme/flash-%'
ORDER BY ts DESC
```

**Coste del día por ruta y motivo de selección** (combinación de eventos):

```sql
SELECT r.route,
       r.model_id,
       SUM(r.cost_usd) AS coste_usd,
       s.reason
FROM "default" AS r
LEFT JOIN "default" AS s
  ON s.event_type = 'selection'
 AND s.route = r.route
 AND s.model_id = r.model_id
WHERE r.event_type = 'request'
  AND r.cost_usd IS NOT NULL
GROUP BY r.route, r.model_id, s.reason
```

## Notas de la auditoría (Grupo 5): gauges de precio

Dos limitaciones del **SDK OTel** (no del gateway) a tener en cuenta al
consultar los gauges de precio:

1. **Series huérfanas** en `llm_gateway_selected_model_price_usd` y
   `llm_gateway_allowlist_price_usd`: el SDK no puede **borrar** series, así
   que cuando un modelo cambia de precio o sale de catálogo, su última serie
   queda publicada para siempre (valor congelado). El valor congelado NO es el
   precio actual. Mitigaciones al consultar:
   - filtrar por `model` concreto en vez de leer todas las series, o
   - cruzar con `llm_gateway_model_selection_total` / los eventos
     `selection` para saber qué serie sigue viva (un `model_id` que deja de
     aparecer en selecciones y desaparece del catálogo —evento `removed`—
     corresponde a series huérfanas).
2. **Desfase del gauge** hasta `refresh_interval_ms`: cuando el refresco del
   catálogo es **on-demand** (disparado por la primera petición tras el TTL,
   el hot path), el gauge de precio del modelo seleccionado queda desfasado
   hasta que ocurre ese primer refresco; con el ticker programado, el desfase
   máximo es `refresh_interval_ms`. En ambos casos, el evento `request` lleva
   el precio **real en el momento de la petición**: para series temporales de
   precio fiables, consulta los eventos `catalog`/`request`, no el gauge.

## Verificación (6.4)

- **Integración**: `tests/observability_events_optout.rs` demuestra que con
  `observability.events: false` una petición completa (catálogo frío →
  selección → respuesta, y su gemela en streaming) no emite ninguno de los
  tres eventos, y el control positivo que con la config por defecto se emiten
  los tres.
- **E2E local** (binario nuevo compilado en release con config scratch): ver
  la salida de `curl /metrics` y los eventos en stdout en el reporte del
  change #39.
- **Consulta SQL contra OpenObserve real**: pendiente de verificación
  post-despliegue — las consultas de esta página se ejecutan sobre eventos
  reales en producción.
