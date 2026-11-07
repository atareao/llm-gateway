# Design: warmup-hardening

## Contexto

La feature `add-backend-warmup` funciona (verificada E2E con Ollama), pero la prueba real reveló dos defectos que los mocks no reproducen y varias asperezas operativas:

- `tokio::time::interval` usa `MissedTickBehavior::Burst` por defecto: si un warmup tarda más que `interval_ms` (cold load largo, timeout de 30 s), el ticker acumula ticks y dispara una ráfaga seguida al terminar. Observado en vivo: ~17 warmups en <0.5 s tras un bloqueo de 30 s.
- `WARMUP_TIMEOUT` fijo de 30 s: el cold load real de un modelo 14B ronda los 90 s, así que los primeros warmups fallan por timeout hasta que el modelo termina de cargar.

## Decisión: `MissedTickBehavior::Delay`

Se fija `ticker.set_missed_tick_behavior(MissedTickBehavior::Delay)`. Con `Delay`, tras un tick tardío el siguiente se programa `interval_ms` después de terminar el anterior, eliminando la ráfaga. Es la semántica correcta para un keep-alive: interesa espaciar peticiones, no recuperar el tiempo perdido.

## Decisión: `timeout_ms` configurable por backend

`Warmup` gana `timeout_ms: Option<u64>`. Si es `None`, se usa `WARMUP_TIMEOUT` (30 s) como default. Así:
- Los backends con modelos grandes configuran un timeout a medida (p. ej. 300 000 ms) y su warmup de arranque no falla por cold load.
- El default conserva el comportamiento actual para quien no lo configure.

Alternativa descartada: subir el default a 300 s para todos — penaliza a los backends pequeños (una conexión colgada bloquea el tick 5 min) y rompe la semántica de "fallo rápido" del resto del gateway (default de tráfico 120 s).

## Decisión: resolución de la API key una vez por tarea

La clave se resuelve al iniciar la tarea de keep-alive, no en cada tick. Motivo: `resolve_api_key` emite un `warn` cuando el `api_key_env` no resuelve; con el keep-alive eso repetiría el aviso en cada intervalo. Resolver una vez:
- Elimina el spam de logs (un `warn` por backend en vez de uno por tick).
- Evita trabajo redundante (lectura de entorno + formateo) por tick.

La variable de entorno no cambia durante la vida del proceso; si no resuelve al arrancar, el warmup registra `failure` una vez y el bucle continúa en `debug` sin emitir HTTP. Se relaja la spec para permitir "como máximo un `failure`" en lugar de uno por tick.

## Decisión: log de causa de error de transporte

`warmup_once` distingue `Ok(resp)` de `Err(err)`; en el `Err` el `warn` incluye el `reqwest::Error` (que ya describe timeout/connect/reset). Facilita el diagnóstico operativo sin añadir campos a las métricas (que se mantienen con cardinalidad acotada `success`/`failure`).

## Decisión: apagado ordenado

`spawn_warmup` ya devuelve `Vec<JoinHandle<()>>`; `main.rs` los conserva, espera `tokio::signal::ctrl_c()` (SIGINT) o SIGTERM y hace `abort()` de cada handle antes de salir. No se hace `await` de los handles tras `abort()` para no bloquear el cierre; `abort()` cancela en el siguiente punto de suspensión.

## Hardening defensivo

`spawn_warmup` ignora un `interval_ms == 0` (`continue`) en lugar de confiar en la precondición de config: `interval` con periodo cero paniquea, y `AppState::new` es público y no valida. No cambia el comportamiento observable (la config ya lo rechaza).

## Testabilidad

- `warmup_once` sigue siendo el punto de prueba determinista contra un mock axum.
- Sin ráfaga: test de integración con `interval_ms` corto y un backend mock que retrasa la respuesta más que el intervalo; se afirma que el nº de peticiones tras la recuperación es ~1, no una ráfaga.
- Timeout configurable: mock que responde tras un retardo mayor que el default; con `timeout_ms` mayor → `success`.
- Aviso único: contador de `warn` no es observable directo; se prueba vía comportamiento (una sola `failure`, y el mock no recibe peticiones).

## Riesgos

- `set_missed_tick_behavior` está disponible desde tokio 1.x; sin coste.
- Esperar señales en `main` añade una rama de cierre; hay que asegurar que el servidor sigue sirviendo mientras se espera (se usa `tokio::select!` o el patrón de `axum::serve(...).with_graceful_shutdown(...)`).
