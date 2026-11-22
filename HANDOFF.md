# Handoff — llm-gateway — 2026-11-22

## Objetivo de la próxima sesión
Implementar **PLAN-008: ruta dinámica OpenRouter free (`openrouter_free`)** — descubrimiento de modelos gratuitos de OpenRouter + ruta con failover/rotación. Diseño ya decidido con el usuario (ver abajo). **Aún NO se ha escrito spec, issue ni código.**

## Estado actual del repo
- Rama actual: `feature/add-localai-upstream` (PLAN-007, **en vuelo**, PR **#30 OPEN** a `development`; 2 commits por delante de `development`).
- Working tree: limpio salvo `HANDOFF.md` (untracked) y `HANDOFF.md.bak-20261122`.
- Servicio `llm-gateway.service` activo; `~/.config/opencode/gateway.yaml` ya contiene la ruta `localai` (línea ~68).
- v0.5.0 desplegada (ver HANDOFF anterior).

## PENDIENTE INMEDIATO (bloquea PLAN-008)
1. Cerrar PLAN-007: revisar/mergear **PR #30** a `development`, archivar `plans/PLAN-007.md` → `PLAN-007-ARCHIVED.md` (si procede).
2. `git checkout development && git pull`.
3. Crear rama `feature/add-openrouter-free-routing` desde `development`.

## PLAN-008 — diseño aprobado por el usuario (decisiones)
- **Alcance:** descubrimiento + ruta dinámica con failover (sin endpoint admin).
- **Política de selección:** solo modelos free capaces de **tool-calling**; orden inicial por `context_length` desc; **rotación round-robin**; **blacklist dinámica** de fallos.
- **Fallback final:** si todos los free agotados → backends estáticos de **pago** en la misma ruta; si no hay, `AllBackendsFailed`.
- **Config:** nuevo tipo `kind: openrouter_free` en `Backend`, mezclable con la lista fija (default = static, compat).

### Config ejemplo
```yaml
route:
  - name: free-coding
    backend:
      - kind: openrouter_free
        base_url: "https://openrouter.ai/api/v1"
        api_key_env: "OPENROUTER_API_KEY"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: 60000
          min_context: 32768
      - base_url: "https://openrouter.ai/api/v1"   # fallback de PAGO
        model: "deepseek/deepseek-v4.1-flash"
        api_key_env: "OPENROUTER_API_KEY"
```

### Diseño técnico
- Descubrimiento: `GET {base_url}/models` (OpenRouter público, sin auth). Filtrar `id` `:free` (o pricing prompt/completion == "0") **y** `supported_parameters` contiene `tools` (y `tool_choice`). Orden `context_length` desc, luego `id`.
- Nuevo módulo `src/discovery.rs` (caché con TTL + refresco perezoso/background).
- Expansión en request: los backends `openrouter_free` se expanden a N backends concretos en su posición con rotación round-robin; luego corre el `route_with_failover` existente (`proxy.rs`).
- Blacklist dinámica: 429/5xx (o `Retry-After`) → modelo fuera por `blacklist_cooldown_ms`.
- Métricas: `llm_gateway_discovery_*` integradas en OTLP/Prometheus.

### Puntos de extensión en código
- `config.rs`: `GatewayConfig/Route/Backend`; añadir `kind` + `discovery`.
- `proxy.rs`: `route_with_failover` (~L284), `is_retryable` (~L517), `list_models` (~L797).
- Nuevo `src/discovery.rs`; patrón de tarea de fondo similar a `warmup.rs`.

## Metodología (SDD + TDD + gitflow)
1. `gh issue create` (feature, `--assignee atareao`) — punto de entrada obligatorio.
2. `openspec new change add-openrouter-free-routing` → `proposal.md` + spec delta (`openspec/specs/gateway/`) + `tasks.md`.
3. `plans/PLAN-008.md` (vía plan-writer, contenido completo).
4. **STOP — esperar aprobación del usuario** antes de escribir código.
5. TDD RED→GREEN→REFACTOR; verificación por CLI (`cargo test`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo fmt --check`).
6. `docs/openrouter-free-routing.md`; archivar plan y change al cerrar.
7. Commits gitmoji; PR `feature/add-openrouter-free-routing` → `development`.

## Punto de partida
```
cd /data/rust/llm-gateway
gh pr view 30            # cerrar PLAN-007
git checkout development && git pull origin development
git checkout -b feature/add-openrouter-free-routing
```

## Riesgos y notas
- Los `:free` de OpenRouter cambian y tienen límites por-modelo; la rotación+blacklist es lo que lo hace viable.
- `OPENROUTER_API_KEY` ya está en `~/.config/opencode/llm-gateway.env`.
- No mezclar esta feature con la rama localai (regla: una tarea, una sesión/rama).
