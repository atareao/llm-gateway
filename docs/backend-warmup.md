# Keep-alive de backends (warmup)

## Qué es y por qué

Un backend de inferencia local (Ollama, llama.cpp, vLLM…) descarga el modelo de
memoria/VRAM tras un periodo de inactividad (`keep_alive` en Ollama vale 5 min
por defecto). La primera petición tras esa descarga paga el **arranque en frío**:
cargar el modelo otra vez, con una latencia de segundos.

El *warmup* o **keep-alive** mantiene el modelo residente enviando de forma
periódica una petición mínima OpenAI-compatible. Cada petición reinicia el
temporizador de inactividad del backend, de modo que nunca se descarga el
modelo. Es **opcional y por backend**: un backend sin bloque `warmup` no recibe
ninguna petición extra.

## Ficheros implicados

- `src/config.rs` — `struct Warmup { interval_ms }` y el campo
  `Backend::warmup: Option<Warmup>`, más la validación `interval_ms > 0`.
- `src/warmup.rs` — `warmup_body`, `warmup_once`, `spawn_warmup`, `WARMUP_TIMEOUT`.
- `src/proxy.rs` — `AppState`, `chat_url`, `resolve_api_key` y `body_for_backend`
  (reutilizados por el warmup, ahora `pub(crate)`).
- `src/metrics.rs` — `record_warmup` y las series `backend_warm` / `warmup_total`.
- `src/main.rs` — llama a `warmup::spawn_warmup(state)` antes de `axum::serve`.
- `tests/warmup.rs` — tests de integración (mock axum que captura peticiones).

## Cómo funciona

- **Warmup de arranque + heartbeat**: `spawn_warmup` lanza una tarea Tokio por
  cada backend con `warmup` presente. El bucle usa `tokio::time::interval`, cuyo
  **primer tick es inmediato** (warmup de arranque); luego repite cada
  `interval_ms`.
- **Petición mínima**: `POST <base_url>/chat/completions` con el `model` real del
  backend, un único mensaje `user` con `"ping"` y `max_tokens: 1`, **sin**
  `stream`. La URL, la sustitución de `model` y el `Authorization: Bearer` se
  resuelven por el mismo camino que el tráfico normal.
- **Semántica warm/cold**: tras cada warmup, el gauge `llm_gateway_backend_warm`
  vale `1` si el backend respondió `2xx` y `0` en caso contrario. `1` significa
  «modelo recién tocado / caliente»; `0`, que el backend no respondió.
- **Un fallo no aborta**: un warmup fallido (backend caído, timeout,
  `api_key_env` sin resolver) se registra como `failure` y el bucle **sigue**. No
  afecta al tráfico normal ni a otros backends.
- **Sin petición sin clave**: si el `api_key_env` del backend no resuelve,
  `warmup_once` devuelve `false` **sin emitir petición HTTP**.

El warmup **no** contamina las métricas de tráfico: no incrementa
`llm_gateway_requests_total` ni `llm_gateway_backend_attempts_total`.

## Configuración

```yaml
route:
  - name: mech-writer
    backend:
      - base_url: "http://localhost:11434/v1"
        model: "qwen2.5:7b-instruct"
        warmup:
          interval_ms: 240000   # 4 min < keep_alive de Ollama (5 min)
```

Structs Rust equivalentes:

```rust
pub struct Warmup {
    /// Intervalo entre warmups, en milisegundos (debe ser mayor que 0).
    pub interval_ms: u64,
}

pub struct Backend {
    pub base_url: String,
    pub model: String,
    pub api_key_env: Option<String>,
    pub timeout_ms: Option<u64>,
    /// Bloque opcional de keep-alive (warmup) para este backend.
    pub warmup: Option<Warmup>,
}
```

Reglas:

- `warmup` es **opcional** por backend; **ausente = deshabilitado**.
- `interval_ms` es obligatorio y debe ser **> 0**; un `0` provoca un error de
  configuración que nombra la ruta y el backend.
- Recomendado: `interval_ms` **menor** que el `keep_alive` del backend (p. ej.
  240 000 ms = 4 min frente a los 5 min de Ollama).

## Métricas

| Métrica | Tipo | Etiquetas |
|---|---|---|
| `llm_gateway_backend_warm` | gauge | `route`, `backend` |
| `llm_gateway_warmup_total` | counter | `route`, `backend`, `result` (`success`, `failure`) |

Las familias solo se exponen si **al menos un backend** declara `warmup`; sin
ningún backend con warmup, no se emiten ni series ni directivas `# HELP`/`# TYPE`.

```bash
curl -s localhost:8080/metrics | grep llm_gateway_backend_warm
```

## Cómo habilitar / desplegar

1. Añade el bloque `warmup` al backend en `gateway.yaml` (ver arriba).
2. Arranca el gateway (`GATEWAY_CONFIG=./gateway.yaml llm-gateway`).
3. Al arrancar ya se dispara el primer warmup; comprueba el gauge:

   ```bash
   curl -s localhost:8080/metrics | grep -E 'backend_warm|warmup_total'
   ```

No hay flag de entorno: el warmup se activa exclusivamente desde el YAML.
Deshabilitarlo = quitar el bloque `warmup` (los backends de la misma ruta sin
`warmup` no se ven afectados).

## Decisiones

- **Neutralidad OpenAI-compatible**: se usa una petición `chat/completions`
  estándar en lugar de la API nativa de cada backend, para no acoplarse a Ollama
  ni a ningún vendor concreto.
- **Deshabilitado por defecto**: sin bloque `warmup`, el comportamiento del
  gateway es idéntico al de antes de esta feature.
- **Reutilización del camino de tráfico**: el warmup comparte helpers con el
  proxy (`chat_url`, `resolve_api_key`, `body_for_backend`), evitando lógica
  duplicada en la resolución de URL y secretos.
- **Cardinalidad acotada**: las etiquetas salen del YAML (rutas y backends), no
  del tráfico.

## Follow-ups y limitaciones

- **Sin shutdown graceful**: `spawn_warmup` devuelve `Vec<JoinHandle<()>>` pero
  `main.rs` los descarta; las tareas quedan *detachadas* y mueren con el proceso.
  Un apagado ordenado requeriría guardarlos y hacer `abort()`/`await` en el cierre.
- **Sin control nativo de `keep_alive`**: no se envía la extensión propietaria de
  Ollama (`keep_alive`), por lo que el periodo efectivo depende del backend; el
  `interval_ms` debe elegirse por debajo de su valor de descarga.
- **Sin verificación con Ollama real**: los tests cubren el comportamiento con
  mocks; falta validar contra un Ollama real (tarea 5.1 del change).
