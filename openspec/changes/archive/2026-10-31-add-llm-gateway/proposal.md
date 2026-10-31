# Proposal: add-llm-gateway

## Intent

Un proxy local **compatible con la API de OpenAI** que enruta cada petición a una **cadena ordenada de backends** (primario local primero, respaldo remoto después) y hace **failover** cuando el primario falla. Motivo: OpenCode V2 fija el modelo de cada agente de forma estática y **no** sabe saltar a otro modelo si el primario falla; este gateway aporta ese *fallback* para poder usar modelos **locales (coste 0)** sin perder robustez.

## Scope

**In:**
- `POST /v1/chat/completions` (streaming SSE y no-streaming), con paso transparente del cuerpo y de los `tool_calls`.
- Failover **antes del primer token** ante fallo de conexión, timeout, `429` o `5xx`.
- `GET /v1/models` y `GET /healthz`.
- Configuración por YAML (`gateway.yaml`, vía `serde-saphyr`).

**Out (explícito):**
- Persistencia, cuotas, métricas avanzadas, UI, multi-tenant y autenticación de entrada.
- Failover **a mitad de stream** (imposible: no se puede retomar un stream ya iniciado).

## Impact

- Nuevo binario Rust `llm-gateway` (axum + tokio + reqwest). Sin daemon Python.
- En OpenCode: un proveedor `gateway` apuntando a `http://127.0.0.1:4000/v1`; los agentes mecánicos usan `model: gateway/<ruta-lógica>`.

## Contratos

- **Config**: `gateway.yaml` (ruta vía env `GATEWAY_CONFIG`, por defecto `./gateway.yaml`).
- **Env**: las claves de API se referencian por `api_key_env` (nunca en el YAML).

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
