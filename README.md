# 🔀 llm-gateway

[![CI](https://github.com/atareao/llm-gateway/actions/workflows/ci.yml/badge.svg)](https://github.com/atareao/llm-gateway/actions/workflows/ci.yml) [![crates.io](https://img.shields.io/crates/v/llm-gateway-rs.svg)](https://crates.io/crates/llm-gateway-rs) [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

> Proxy local **compatible con la API de OpenAI** que enruta cada petición a una **cadena ordenada de backends** (local primero, remoto de respaldo) y hace **failover** cuando el primario falla.

Escrito en **Rust** (axum + tokio + reqwest). Pensado para que [OpenCode](https://opencode.ai) pueda usar modelos **locales** (coste 0) sin perder robustez: OpenCode fija el modelo de cada agente de forma estática y **no** salta a otro si el primario falla; este gateway aporta ese *fallback*.

## ✨ Características

| | |
|---|---|
| 🔀 **Failover** | Cadena ordenada de backends por ruta; salta al siguiente ante fallo de conexión, timeout, `429` o `5xx` |
| 🧩 **OpenAI-compatible** | `POST /v1/chat/completions` (stream y no-stream), `GET /v1/models`, `GET /healthz` |
| 🛠️ **Tools** | Paso transparente de `tools`, `tool_choice` y `tool_calls` |
| 📡 **Streaming SSE** | Reenvío sin bufferizar de los chunks `data:` |
| 🔑 **Secretos por entorno** | Las claves se referencian con `api_key_env`; nunca en el YAML ni en los logs |
| 📊 **Observabilidad** | `/metrics` en formato Prometheus, logs correlacionados por `x-request-id` y salida JSON opcional |

## 🚧 Estado

En desarrollo (SDD/OpenSpec). El diseño y los escenarios viven en [`openspec/changes/add-llm-gateway/`](./openspec/changes/add-llm-gateway/).

## 📥 Instalar

Desde crates.io (instala el binario `llm-gateway`):

```bash
cargo install llm-gateway-rs
```

O descarga un binario precompilado desde [Releases](https://github.com/atareao/llm-gateway/releases) (`llm-gateway-x86_64-unknown-linux-gnu.tar.gz` / `llm-gateway-aarch64-unknown-linux-gnu.tar.gz`).

## 📦 Compilar

```bash
cargo build --release
```

## ⚙️ Configuración (`gateway.yaml`)

```yaml
route:
  - name: mech-writer
    backend:
      - base_url: "http://localhost:11434/v1"
        model: "qwen2.5:7b-instruct"
      - base_url: "https://openrouter.ai/api/v1"
        model: "qwen/qwen3-8b"
        api_key_env: "OPENROUTER_API_KEY"
        timeout_ms: 30000
```

> `timeout_ms` es **opcional** por backend. En no-stream es el **timeout total** de
> la petición; en streaming limita el **primer byte**. Si se omite, se usa el
> default correspondiente (ver más abajo).

## 🔀 Enrutado y failover

Cada petición trae un `model` **lógico** (nombre de ruta). El gateway lo sustituye por el `model` real del backend elegido y reenvía el resto del cuerpo verbatim. Los backends de una ruta se prueban **en orden** (primario primero).

- **Failover antes del primer byte**: si un backend falla por conexión, timeout, `429` o `5xx`, se salta al siguiente. Un `4xx` distinto de `429` se devuelve tal cual, sin reintentar. Si todos fallan → `502` estilo OpenAI.
- **Timeout (no-stream)**: se aplica **siempre** un timeout total. Prioridad: `timeout_ms` del backend si está definido; en caso contrario, el default **120 s** (`DEFAULT_REQUEST_TIMEOUT`). Así una respuesta no-stream colgada nunca deja la petición abierta indefinidamente.
- **Timeout (streaming)**: el `timeout_ms` (o el default de **primer byte**, 5 s) solo limita el arranque. Una vez llegan las cabeceras, el cuerpo SSE fluye sin deadline, para no abortar respuestas largas.
- **Cabecera `x-gateway-backend`**: en las respuestas con éxito identifica el backend que atendió la petición, con el formato **`<base_url>#<model>`** (p. ej. `http://localhost:11434/v1#qwen2.5:7b-instruct`).
- **Streaming**: con `"stream": true` los chunks SSE `data:` se reenvían sin bufferizar y terminan cuando el upstream cierra (típicamente `data: [DONE]`).
- **⚠️ Limitación (corte a mitad de stream)**: una vez empezada la emisión **no hay failover**. Si el upstream se corta a mitad, el stream se cierra con error; no se reintenta porque ya se habrían enviado bytes al cliente.
- **Secretos**: los backends con `api_key_env` leen la clave del entorno en tiempo de petición y envían `Authorization: Bearer <valor>`. Si la variable no está definida, el backend se considera no disponible y se salta. La clave **nunca** aparece en los logs.

## ♨️ Keep-alive de backends (warmup)

Mantiene **caliente** un backend OpenAI-compatible para evitar el **arranque en frío** (modelo no residente en memoria/VRAM). Al arrancar y cada `interval_ms` el gateway envía un `POST <base_url>/chat/completions` mínimo (un mensaje `user`, `max_tokens: 1`, sin `stream`) que reinicia el temporizador de inactividad del backend (p. ej. el `keep_alive` de Ollama, 5 min).

```yaml
route:
  - name: mech-writer
    backend:
      - base_url: "http://localhost:11434/v1"
        model: "qwen2.5:7b-instruct"
        warmup:
          interval_ms: 240000   # 4 min < keep_alive de Ollama (5 min)
          timeout_ms: 300000    # opcional; default 30 s si se omite
```

- `warmup` es **opcional** por backend; **ausente = deshabilitado** (comportamiento sin cambios).
- `interval_ms` es obligatorio y debe ser **> 0** (un `0` es error de configuración).
- `timeout_ms` es opcional y debe ser **> 0** si está presente; acota cada warmup. Si se omite, se usa el default de 30 s (súbelo para modelos con cold load largo, p. ej. ~90 s en un 14B).
- Un warmup fallido **no** aborta el keep-alive ni afecta a los demás backends. Tras un warmup más largo que `interval_ms` no hay ráfaga de peticiones (los ticks acumulados se descartan).
- Al recibir SIGINT/SIGTERM el gateway aborta las tareas de keep-alive y cierra de forma ordenada.

## 📊 Observabilidad

### Métricas Prometheus

`GET /metrics` expone el estado del gateway en formato de exposición de Prometheus
(`text/plain; version=0.0.4`). No requiere autenticación: el gateway escucha en
loopback por defecto.

| Métrica | Tipo | Etiquetas |
|---|---|---|
| `llm_gateway_requests_total` | counter | `route`, `streaming`, `outcome` (`success`, `all_failed`) |
| `llm_gateway_request_duration_seconds` | histogram | `route`, `streaming` |
| `llm_gateway_failover_total` | counter | `route` |
| `llm_gateway_backend_attempts_total` | counter | `route`, `backend`, `result` |
| `llm_gateway_backend_duration_seconds` | histogram | `route`, `backend` |
| `llm_gateway_backend_warm` | gauge | `route`, `backend` |
| `llm_gateway_warmup_total` | counter | `route`, `backend`, `result` |
| `llm_gateway_errors_total` | counter | `kind` (`invalid_request`, `model_not_found`) |

`result` vale `success`, `retryable_status`, `transport_error`,
`first_byte_timeout`, `missing_api_key`, `body_error` o `no_tool_call` (este
último es una respuesta `2xx` sin `tool_calls` cuando la petición exigía una
llamada a herramienta; ver [`docs/toolcall-failover.md`](docs/toolcall-failover.md).
Los histogramas usan los buckets `0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10, 30, 60,
120` segundos y `+Inf`.

En streaming, los histogramas de duración miden el **tiempo hasta la primera
cabecera** (TTFB), no la duración total del stream: el gateway deja de medir en
cuanto el upstream entrega las cabeceras.

La cardinalidad está acotada por la configuración: solo se etiqueta con rutas y
backends del YAML, nunca con el `model` que pide el cliente.

```bash
curl -s localhost:8080/metrics | grep llm_gateway_failover_total
```

### Correlación de logs

Cada petición a `/v1/chat/completions` devuelve `x-request-id`. Si el cliente
envía uno válido (ASCII visible, 1–128 bytes) se reutiliza; si no, se genera
`req-<pid>-<contador>`. Todos los eventos de esa petición llevan el mismo
`request_id`, junto con `route`, `streaming`, `outcome`, `attempts` y
`latency_ms`; cada intento añade `backend`, `result` y su `latency_ms`.

### Formato de logs

```bash
GATEWAY_LOG_FORMAT=json llm-gateway   # una línea JSON por evento
RUST_LOG=debug llm-gateway            # incluye los intentos con éxito
```

## ▶️ Uso

```bash
GATEWAY_CONFIG=./gateway.yaml llm-gateway
# Escucha por defecto en http://127.0.0.1:8080
# Sobrescribe con GATEWAY_LISTEN=host:puerto
```

En OpenCode, como proveedor:

```jsonc
"gateway": {
  "package": "aisdk:@ai-sdk/openai-compatible",
  "settings": { "baseURL": "http://127.0.0.1:8080/v1" },
  "models": { "mech-writer": { "capabilities": { "tools": true } } }
}
```

## 🔁 Git Flow

Este proyecto sigue **Git Flow** (`main` / `development` / `feature/*` / `hotfix/*`) con conventional commits + gitmoji. Ver [GIT_FLOW.md](./GIT_FLOW.md).

## 📄 Licencia

MIT © 2026 Lorenzo Carbonell
