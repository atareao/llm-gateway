# Proposal

## Why

En modo **no-stream**, si un backend no declara `timeout_ms` el gateway **no aplica ningún timeout** a la petición (solo el `connect_timeout` de 4 s). Un backend que acepta la conexión pero nunca responde deja la petición **colgada indefinidamente** y **no dispara el failover**, rompiendo la promesa de robustez del gateway. En **streaming** ya existe un presupuesto por defecto (`DEFAULT_FIRST_BYTE_TIMEOUT`); en no-stream falta el equivalente.

## What Changes

- Añadir un **timeout total por defecto** (`DEFAULT_REQUEST_TIMEOUT`) para peticiones **no-stream** cuando el backend no declara `timeout_ms`, con el mismo patrón que `DEFAULT_FIRST_BYTE_TIMEOUT` en streaming.
- Que el vencimiento de ese timeout cuente como **fallo antes del primer byte** → failover al siguiente backend.
- **Sin cambios de formato de configuración** (no **BREAKING**): `timeout_ms` sigue siendo opcional y, si se declara, **tiene prioridad** sobre el default.
- Documentar el comportamiento en la spec `gateway`, el `README.md` y `gateway.yaml.example`.

## Capabilities

### New Capabilities
<!-- Ninguna: no se introduce capability nueva. -->

### Modified Capabilities
- `gateway`: se añade un requisito que fija el **presupuesto de timeout por defecto** de los backends sin `timeout_ms` (timeout **total** en no-stream) y se documenta el de **primer byte** en streaming (ya existente en código).

## Impact

- **Código**: `src/proxy.rs` (constante `DEFAULT_REQUEST_TIMEOUT` y rama no-stream de `send`).
- **Spec**: `openspec/specs/gateway/spec.md` (al archivar el cambio).
- **Docs**: `README.md`, `gateway.yaml.example`.
- **Sin cambios** en `src/config.rs`, en el formato YAML ni en la API pública.
