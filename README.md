# 🔀 llm-gateway

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

## 🚧 Estado

En desarrollo (SDD/OpenSpec). El diseño y los escenarios viven en [`openspec/changes/add-llm-gateway/`](./openspec/changes/add-llm-gateway/).

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

## 🔀 Enrutado y failover

Cada petición trae un `model` **lógico** (nombre de ruta). El gateway lo sustituye por el `model` real del backend elegido y reenvía el resto del cuerpo verbatim. Los backends de una ruta se prueban **en orden** (primario primero).

- **Failover antes del primer byte**: si un backend falla por conexión, timeout, `429` o `5xx`, se salta al siguiente. Un `4xx` distinto de `429` se devuelve tal cual, sin reintentar. Si todos fallan → `502` estilo OpenAI.
- **Cabecera `x-gateway-backend`**: en las respuestas con éxito identifica el backend que atendió la petición, con el formato **`<base_url>#<model>`** (p. ej. `http://localhost:11434/v1#qwen2.5:7b-instruct`).
- **Streaming**: con `"stream": true` los chunks SSE `data:` se reenvían sin bufferizar y terminan cuando el upstream cierra (típicamente `data: [DONE]`).
- **⚠️ Limitación (corte a mitad de stream)**: una vez empezada la emisión **no hay failover**. Si el upstream se corta a mitad, el stream se cierra con error; no se reintenta porque ya se habrían enviado bytes al cliente.
- **Secretos**: los backends con `api_key_env` leen la clave del entorno en tiempo de petición y envían `Authorization: Bearer <valor>`. Si la variable no está definida, el backend se considera no disponible y se salta. La clave **nunca** aparece en los logs.

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
