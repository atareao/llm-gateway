# LocalAI como upstream del llm-gateway

## Qué es

LocalAI v4.12.1 corre como servicio de usuario systemd en `127.0.0.1:9090` (backend CUDA 13, RTX 4060 Ti) y sirve modelos locales. Se integra en el gateway como un upstream OpenAI-compatible más, mediante la ruta dedicada `localai`.

## Ficheros implicados

- Config de runtime: `~/.config/opencode/gateway.yaml` (ruta `localai`; NO vive en el repo).
- Ejemplo: `gateway.yaml.example`.
- Unit LocalAI: `~/.config/systemd/user/local-ai.service`.
- Unit gateway: `~/.config/systemd/user/llm-gateway.service`.

## Cómo funciona

La ruta `localai` declara una cadena ordenada: LocalAI (`http://127.0.0.1:9090/v1`, modelo `qwen3-1.7b`) como primario y OpenRouter (`deepseek/deepseek-v4.1-flash`) como fallback. El gateway aplica su failover habitual antes del primer token.

Se usa una ruta dedicada (y no un fallback intermedio en las rutas Ollama) para no forzar la coexistencia de dos modelos pesados en VRAM sobre 16 GB: `qwen3-1.7b` (~2,2 GB) cabe siempre junto a Ollama.

```yaml
route:
  - name: localai
    backend:
      - base_url: "http://127.0.0.1:9090/v1"
        model: "qwen3-1.7b"
        timeout_ms: 120000
      - base_url: "https://openrouter.ai/api/v1"
        model: "deepseek/deepseek-v4.1-flash"
        api_key_env: "OPENROUTER_API_KEY"
        timeout_ms: 120000
```

## Variables / configuración

- No requiere variables nuevas. El fallback usa `OPENROUTER_API_KEY` (ya presente en `~/.config/opencode/llm-gateway.env`).
- `model` debe coincidir con un modelo servido por LocalAI (ver `curl http://127.0.0.1:9090/v1/models`).

## Cómo habilitar / desplegar

1. Añadir la ruta a `~/.config/opencode/gateway.yaml`.
2. `systemctl --user restart llm-gateway`.
3. Verificar: `curl -s http://127.0.0.1:4010/v1/models | rg localai`.

## Verificación

- `curl` a `/v1/models` incluye `localai`.
- Inferencia real a través del gateway con `finish_reason: stop`.
- Failover: con LocalAI detenido, responde OpenRouter.

## Decisiones y follow-ups

- Decisión: ruta dedicada, no intermedia (VRAM).
- Follow-up: warmup/keep-alive de LocalAI si molesta el cold start (~20 s).
- Trampa conocida: LocalAI mantiene modelos residentes hasta que actúa su watchdog (5m); con contexto 32k solo cabe un modelo pesado a la vez.
