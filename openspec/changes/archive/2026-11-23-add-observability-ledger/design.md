# Design

## Context

Ver `proposal.md` — Why. Estado actual relevante:

- `src/telemetry.rs` construye el stack OpenTelemetry: Prometheus (fuente única de `GET /metrics`) y, según `signals.metrics` / `signals.logs` / `signals.traces`, exportadores OTLP hacia OpenObserve. Los logs viajan por `opentelemetry-appender-tracing`.
- El gateway **no parsea `usage`** (no hay `prompt_tokens`/`completion_tokens` en el código), por lo que hoy no puede calcular coste por petición.
- `src/discovery.rs` refresca el catálogo en background (`spawn_refresh`) y `ModelEntry` ya expone `pricing` y `context_length`; `classify`/expansión ya seleccionan modelos por ruta.
- #37 introduce el gate de cuota free, el respaldo `openrouter_paid` y la **selección pegajosa**; sus transiciones (elección inicial, más barato, failover, baja) son precisamente los eventos de selección que este change registra.

## Goals / Non-Goals

**Goals**

- Registrar la **evolución de precios** del catálogo de forma consultable y durable.
- Registrar **por qué** cambió el modelo elegido en cada ruta y con qué precio.
- Registrar **tokens, coste y latencia** por petición, con coste real (no estimado a ciegas).
- Cero base de datos local: todo en OpenObserve.
- No penalizar el hot path (emisión asíncrona, sin bloqueos).

**Non-Goals**

- No se introduce SQLite ni un libro mayor relacional (follow-up si algún día hace falta histórico exacto eterno e independiente de OpenObserve).
- No se implementan evaluaciones de calidad automáticas; los proxies de "resultado" son `finish_reason`, cumplimiento de tool-calls, reintentos y tasa de error.
- No se deduplican eventos entre reinicios (el dedup del catálogo es en memoria; tras reiniciar se reemite el snapshot completo, que es barato).

## Decisions

### D1: OpenObserve-only, sin SQLite

OpenObserve es un almacén de logs con motor SQL, retención y dashboards. Los tres eventos son consultables con SQL directamente, sin dependencia nueva ni migraciones. SQLite solo se justificaría si se quisiera un histórico exacto e independiente de la disponibilidad/retención de OpenObserve.

- **Alternativa descartada**: SQLite (hibrido) → más piezas y migraciones para un uso personal.

### D2: Log-on-change para catálogo y selección

`gateway_catalog` y `gateway_selection` se emiten **solo cuando algo cambia**: el gateway mantiene en memoria el último snapshot de precios (por `model_id`) y el último modelo elegido (por `route`/`backend`), y solo emite si difiere. Los precios cambian poco y las selecciones menos: el volumen es mínimo.

- **Alternativa descartada**: emitir un evento por modelo en cada refresco → ruido diario sin valor.

### D3: Evento de petición denormalizado

`gateway_requests` incluye ya `model_id`, `pricing.prompt`, `pricing.completion`, `tokens_in`, `tokens_out` y `cost_usd` calculado. Así el coste por modelo y día se obtiene agregando **un solo** stream, sin *joins* entre streams (limitados en OpenObserve).

### D4: Parseo de `usage` como prerrequisito

- **No-stream**: el gateway ya lee el body (para `tool_calls`); se extrae también `usage`.
- **Stream**: se inyecta `"stream_options": {"include_usage": true}` en la petición y se lee el `usage` del último chunk antes de `[DONE]`.
- Si no hay `usage`, los tokens y el `cost_usd` quedan **desconocidos** (nulos), nunca un cero inventado.

### D5: Eventos como log estructurado, con `event_type`

Los tres eventos se emiten como log estructurado con un campo `event_type` (`catalog` | `selection` | `request`) y campos tipados. Si la retención por stream importa, OpenObserve puede separarlos en streams mediante una *pipeline* que enrute por `event_type`; si no, conviven en un stream filtrable por `event_type`. Ambas opciones se documentan.

### D6: Cardinalidad de métricas

El label `model` es aceptable (~500 series); `route` también. **Nunca** se usa `request_id` como label de métrica (eso vive en el evento de petición, no en Prometheus/OTLP métricas).

### D7: Opt-out

Bloque `observability` con `events: bool` (default `true`). Desactivarlo detiene la emisión de los tres eventos (las métricas siguen el control existente `signals.metrics`).

## Risks / Trade-offs

- [Retención y disponibilidad de OpenObserve] → OpenObserve poda por política y, si está caído, el exportador OTel por lotes descarta ese tramo. Mitigación: retención larga para `catalog`/`selection` (1-2 años) y 30-90 días para `requests`; aceptar la pérdida de tramos si OpenObserve cae (uso personal).
- [Cardinalidad] → `model` como label acota a las ~500 series del catálogo; se prohíbe `request_id` en métricas.
- [Coste de parseo/emisión en el hot path] → el parseo de `usage` es O(1) sobre un body ya leído; la emisión es un log (no bloqueante). En streaming el `include_usage` añade un chunk final mínimo.
- [`usage` ausente (streaming sin soporte)] → tokens y coste desconocidos; el evento se emite igual con nulos, sin romper la respuesta al cliente.
- [Emisión incompleta en streaming] → un aborto del cliente a mitad de stream descarta el body antes del fin real y **no emite** `gateway_requests`; un corte del upstream a mitad tampoco emite (el stream termina en error), o emite con datos parciales si el upstream cierra limpio pero incompleto. Mitigación: pérdida aceptada (uso personal); el evento nunca inventa ceros.
- [Volumen de `requests`] → proporcional al tráfico; se mitiga con retención corta y con la denormalización (un evento por petición, sin eventos adicionales por intento).

## Migration Plan

1. Desplegar con `observability.events: true` (default) y `signals.logs` apuntando a OpenObserve.
2. En OpenObserve, fijar la retención por stream (o por pipeline): `catalog`/`selection` 1-2 años, `requests` 30-90 días.
3. Verificar: `curl -s http://127.0.0.1:4010/metrics | rg 'tokens_total|cost_usd'` y una consulta de coste por modelo y día en OpenObserve.
- **Rollback**: `observability.events: false` o revertir el binario.

## Open Questions

Ninguna.
