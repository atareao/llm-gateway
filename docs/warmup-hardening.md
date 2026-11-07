# Warmup hardening

## Qué es

Endurecimiento del *keep-alive* (warmup) de backends tras la verificación real
con Ollama (5.1). Corrige dos defectos que los mocks no reproducían y pule el
resto de hallazgos del reviewer:

1. **Ráfaga de warmups** tras un warmup lento (`MissedTickBehavior::Burst`).
2. **`WARMUP_TIMEOUT` fijo (30 s)** insuficiente para el cold load de modelos
   grandes (~90 s en un 14B).
3. **Spam de `warn`** cuando `api_key_env` no resuelve (uno por tick).
4. **Diagnóstico pobre** del fallo de transporte (sin causa en el log).
5. **Sin apagado ordenado** de las tareas de keep-alive.

Change OpenSpec: `openspec/changes/warmup-hardening/` (proposal, design, spec
delta, tasks). Plan: `plans/PLAN-002.md`.

## Ficheros implicados

- `src/config.rs` — `Warmup.timeout_ms: Option<u64>` + validación `> 0`.
- `src/warmup.rs` — `WARMUP_TIMEOUT`, `warmup_timeout`, `warmup_request`,
  `warmup_once`, `spawn_warmup`.
- `src/main.rs` — conserva los `JoinHandle` y los aborta en SIGINT/SIGTERM.
- `src/proxy.rs` — reutilizado tal cual (`resolve_api_key`, `body_for_backend`,
  `chat_url`).
- `tests/config.rs`, `tests/warmup.rs` — nuevos escenarios.
- `docs/backend-warmup.md`, `gateway.yaml.example`, `README.md` — docs.

## Cómo funciona

- **`timeout_ms` opcional**: cada warmup usa `warmup.timeout_ms` o, si se omite,
  `WARMUP_TIMEOUT` (30 s). `warmup_timeout(&Warmup) -> Duration` centraliza la
  regla; un `timeout_ms` presente debe ser `> 0` (validado al cargar config).
- **`MissedTickBehavior::Delay`**: `spawn_warmup` lo fija en el `ticker`. Tras un
  warmup más largo que `interval_ms`, el siguiente tick se programa `interval_ms`
  después de terminar el actual, en lugar de disparar los ticks acumulados.
- **Guardia `interval.is_zero()`**: `spawn_warmup` ignora (`continue`) un
  `interval_ms == 0` en lugar de confiar en la precondición (`interval` con
  periodo cero paniquea y `AppState::new` es público).
- **Clave resuelta una vez**: `resolve_api_key` se llama al iniciar la tarea, no
  por tick. Si no resuelve, se registra **un único** `failure` (y el `warn` de
  `resolve_api_key` una vez) y el bucle continúa en `debug` sin peticiones HTTP.
- **Causa del fallo de transporte**: `warmup_request` distingue `Ok`/`Err`; en el
  `Err` el `warn` incluye `error = %err` (el `reqwest::Error` describe
  timeout/connect/reset).
- **Apagado ordenado**: `main.rs` usa
  `axum::serve(...).with_graceful_shutdown(shutdown_signal())` y, al terminar,
  `abort()` de cada handle de warmup. `shutdown_signal` espera SIGINT (Ctrl-C) o
  SIGTERM.

## Variables / configuración

```yaml
warmup:
  interval_ms: 240000
  timeout_ms: 300000   # opcional; default 30 s
```

Sin variables de entorno nuevas. `timeout_ms` ausente ⇒ `WARMUP_TIMEOUT` (30 s).

## Cómo habilitar / desplegar

Se activa desde el YAML (sin flags). Para un modelo con cold load largo,
configura `warmup.timeout_ms` por encima de ese tiempo (p. ej. 300 000 ms). El
cierre del proceso (Ctrl-C o `kill -TERM`) aborta las tareas de keep-alive.

## Decisiones

- **`Delay`** en vez de `Burst`: un keep-alive interesa espaciar peticiones, no
  recuperar el tiempo perdido.
- **Timeout por backend** en vez de subir el default: evita penalizar a backends
  pequeños y mantiene la semántica de fallo rápido del resto del gateway.
- **Resolución única de la clave**: la variable de entorno no cambia en la vida
  del proceso.
- **Cardinalidad acotada**: las métricas de warmup siguen en `success`/`failure`;
  la causa del error va al log, no a una etiqueta.

## Follow-ups

- Verificación manual 5.1 contra Ollama real (`qwen3:14b`) con `timeout_ms`
  acorde al cold load (~90 s).
- Sin control nativo de `keep_alive` de Ollama (documentado en
  `docs/backend-warmup.md`).
