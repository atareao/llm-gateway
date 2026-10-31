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

## ▶️ Uso

```bash
GATEWAY_CONFIG=./gateway.yaml llm-gateway
# Escucha en http://127.0.0.1:4000/v1
```

En OpenCode, como proveedor:

```jsonc
"gateway": {
  "package": "aisdk:@ai-sdk/openai-compatible",
  "settings": { "baseURL": "http://127.0.0.1:4000/v1" },
  "models": { "mech-writer": { "capabilities": { "tools": true } } }
}
```

## 🔁 Git Flow

Este proyecto sigue **Git Flow** (`main` / `development` / `feature/*` / `hotfix/*`) con conventional commits + gitmoji. Ver [GIT_FLOW.md](./GIT_FLOW.md).

## 📄 Licencia

MIT © 2026 Lorenzo Carbonell
