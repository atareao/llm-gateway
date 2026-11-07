# Design: add-backend-warmup

## Contexto

El gateway reenvía `POST /v1/chat/completions` a backends OpenAI-compatible con failover antes del primer byte. No interpreta el cuerpo salvo `model`. Los backends locales pueden descargar el modelo de VRAM por inactividad.

## Decisión: warmup genérico vía chat/completions

Se reenvía una petición mínima al mismo endpoint que el tráfico real (`POST <base_url>/chat/completions`) con `max_tokens: 1`, un único mensaje `user` y sin `stream`. Ventajas:
- Genérico para cualquier backend OpenAI-compatible (no depende de la API nativa de Ollama).
- Cada petición reinicia el temporizador de inactividad del backend (p. ej. `keep_alive` de Ollama, 5 min), de modo que un `interval_ms` menor mantiene el modelo caliente.

## Decisión: opt-in por backend

`warmup` es opcional y por backend. Normalmente solo el backend local primario lo necesita; los remotos (p. ej. OpenRouter) no. Ausente = deshabilitado = comportamiento actual intacto.

## Decisión: tarea de fondo con `tokio::time::interval`

`interval` dispara el primer tick de inmediato, lo que da el warmup de arranque sin lógica extra; después repite cada `interval_ms`. Un warmup fallido se registra (`warn` + `result="failure"`, `backend_warm=0`) y no aborta el bucle.

## Decisión: estado warm/cold

El gateway no puede leer la VRAM del backend; define "warm" como "el último warmup respondió 2xx". El gauge arranca en 0 y se actualiza en cada intento.

## Reutilización

`warmup.rs` reutiliza `chat_url`, `resolve_api_key` y `body_for_backend` de `proxy.rs` (pasan a `pub(crate)`) para no duplicar el cálculo de URL, la resolución de secretos ni la sustitución de `model`.

## Testabilidad

`warmup_once` es una función pública que ejecuta un único intento: los tests la invocan contra un mock axum y afirman sobre la petición recibida, el valor de retorno y las métricas, sin depender de temporizadores. `spawn_warmup` se prueba con un `interval_ms` corto.

## Riesgos

- Cardinalidad de métricas: acotada por la config (solo rutas/backends del YAML).
- Fuga de la API key: el warmup usa el mismo camino que el tráfico normal; jamás loguea el valor de la clave.
