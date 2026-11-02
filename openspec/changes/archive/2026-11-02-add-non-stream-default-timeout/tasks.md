# Tasks

## 1. RED — tests que fallan

- [x] 1.1 Test de integración no-stream: primario **sin `timeout_ms`** que acepta la conexión y **nunca responde** → el gateway falla por timeout y responde desde el fallback (`x-gateway-backend` = fallback).
- [x] 1.2 Test no-stream: ruta con único backend sin `timeout_ms` que nunca responde → `502` estilo OpenAI (no cuelga).
- [x] 1.3 Test no-stream con `timeout_ms` explícito corto → se respeta ese valor (no se usa el default).
- [x] 1.4 Ejecutar `cargo test` y confirmar que 1.1–1.3 fallan (y el resto sigue **verde**).

## 2. GREEN — implementación mínima

- [x] 2.1 Añadir la constante `DEFAULT_REQUEST_TIMEOUT` (120 s) en `src/proxy.rs`.
- [x] 2.2 En `send` (rama no-stream), aplicar **siempre** un timeout: `timeout_ms` si existe, si no `DEFAULT_REQUEST_TIMEOUT`.
- [x] 2.3 `cargo test` en verde (100%).
- [x] 2.4 `cargo check` sin errores.

## 3. REFACTOR

- [x] 3.1 `cargo fmt --check`.
- [x] 3.2 `cargo clippy -- -D warnings` sin avisos.
- [x] 3.3 `cargo test` de nuevo (sin regresiones).

## 4. Docs

- [x] 4.1 `README.md`: documentar el timeout por defecto en no-stream y la prioridad de `timeout_ms`.
- [x] 4.2 `gateway.yaml.example`: nota del default.
- [x] 4.3 Doc-comment de `send` y de la nueva constante.

## 5. Verificación manual (end-to-end)

- [x] 5.1 Con un backend no-stream colgado (p. ej. `nc -l` aceptando pero sin responder), comprobar el failover real y `x-gateway-backend`.
- [x] 5.2 Tras aprobación y verificación: `openspec archive add-non-stream-default-timeout`.

## 6. Git Flow

- [x] 6.1 Crear `feature/add-non-stream-default-timeout` desde `development`.
- [ ] 6.2 Commits con gitmoji + PR a `development`.
