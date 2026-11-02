## ADDED Requirements

### Requirement: Tests de integración deterministas

Los tests de integración SHALL simular un backend caído sin liberar el puerto efímero reservado, de forma que ningún otro test pueda enlazarlo y alterar el escenario.

El helper compartido `tests/common/mod.rs` SHALL exponer `pub async fn dead_backend() -> String`, que devuelve una `base_url` cuyo listener permanece enlazado y cierra cada conexión aceptada sin responder.

**Scenarios:**

#### Scenario: Puerto del backend caído no reutilizable
- **GIVEN** el `base_url` devuelto por `dead_backend()`
- **WHEN** otro componente intenta `TcpListener::bind` sobre esa misma dirección
- **THEN** el bind falla porque el puerto sigue ocupado

#### Scenario: Failover ante backend caído
- **GIVEN** una ruta con primario `dead_backend()` y fallback OK
- **WHEN** llega `POST /v1/chat/completions`
- **THEN** responde `200` con `x-gateway-backend` del fallback
- **AND** el test es estable en ejecuciones repetidas y en paralelo

#### Scenario: Sin duplicación del helper
- **GIVEN** los ficheros `tests/failover.rs`, `tests/stream.rs`, `tests/integration.rs` y `tests/secrets.rs`
- **WHEN** se compilan los tests
- **THEN** todos usan `common::dead_backend` y no queda ninguna copia local
