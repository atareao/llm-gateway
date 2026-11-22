# PLAN-007 — Ruta dedicada `localai` (upstream LocalAI en `:9090`)

- **Issue:** [#29](https://github.com/atareao/llm-gateway/issues/29)
- **Rama:** `feature/add-localai-upstream`
- **Vía:** ligera (sin OpenSpec/TDD: cambio de configuración + documentación, sin código)

## Objetivo

Integrar **LocalAI** (`127.0.0.1:9090`) como upstream del `llm-gateway` mediante una **ruta dedicada `localai`**: primario LocalAI con `qwen3-1.7b` → fallback OpenRouter (`deepseek/deepseek-v4.1-flash`). No se tocan las rutas Ollama existentes.

## Alcance

- Config de runtime (fuera del repo): añadir la ruta `localai` a `~/.config/opencode/gateway.yaml`.
- `gateway.yaml.example`: documentar el patrón de upstream OpenAI-compatible de terceros (LocalAI).
- `docs/localai-upstream.md`: documentación de la feature.
- `plans/PLAN-007.md` (este plan).

## Fuera de alcance

- Cambios en `src/` (el gateway ya es un proxy OpenAI-compatible genérico).
- OpenSpec y TDD (descartados para este caso: no hay comportamiento nuevo).
- Insertar LocalAI como fallback intermedio en las rutas Ollama.
- Tocar `llm-gateway.env` ni las API keys.
- Warmup/keep-alive de LocalAI (follow-up si molesta el cold start).

## Decisión de diseño

Ruta dedicada en lugar de fallback intermedio: evita forzar la coexistencia de dos modelos pesados en VRAM (Ollama + LocalAI sobre 16 GB). `qwen3-1.7b` (~2,2 GB) cabe siempre junto a Ollama; el failover a OpenRouter replica el contrato del resto de rutas.

## Tareas

1. Añadir la ruta `localai` a `~/.config/opencode/gateway.yaml`.
2. Documentar el upstream LocalAI en `gateway.yaml.example`.
3. Crear `docs/localai-upstream.md`.
4. `systemctl --user restart llm-gateway`; verificar `/v1/models` (incluye `localai`), inferencia real (`finish_reason: stop`) y failover a OpenRouter con LocalAI caído.
5. Cierre: renombrar a `plans/PLAN-007-ARCHIVED.md`, commit gitmoji en `feature/add-localai-upstream` y PR a `development`.

## Criterio de cierre

- `/v1/models` expone la ruta `localai`.
- Inferencia real a través del gateway por la ruta `localai` con `finish_reason: stop`.
- Failover a OpenRouter verificado con LocalAI detenido.
- `systemctl --user restart llm-gateway` sin errores en el journal.
- `docs/localai-upstream.md` publicado y `gateway.yaml.example` actualizado.
- PR mergeado a `development` y plan archivado.