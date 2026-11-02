# Spec Delta

## ADDED Requirements

### Requirement: Presupuesto de timeout por defecto sin `timeout_ms`

Cuando un backend no declara `timeout_ms`, el gateway SHALL aplicar un presupuesto por defecto que acote la petición y permita disparar el failover. El presupuesto SHALL diferir según el modo:

- **No-stream**: un **timeout total** de la petición (`DEFAULT_REQUEST_TIMEOUT`), que abarca desde el establecimiento de la conexión hasta el último byte del cuerpo.
- **Stream**: un **time-to-first-byte** (`DEFAULT_FIRST_BYTE_TIMEOUT`), que acota únicamente el arranque; una vez recibidas las cabeceras, el stream fluye sin deadline.

Si el backend **sí** declara `timeout_ms`, ese valor SHALL tener prioridad sobre el presupuesto por defecto correspondiente. El vencimiento del presupuesto SHALL considerarse un fallo **antes del primer byte**, elegible para failover.

**Scenarios:**

#### Scenario: No-stream sin `timeout_ms` responde desde el fallback
- **GIVEN** una ruta cuyo primario no declara `timeout_ms` y acepta la conexión pero nunca responde
- **AND** un fallback que responde con normalidad
- **WHEN** llega una petición no-stream
- **THEN** al vencer `DEFAULT_REQUEST_TIMEOUT` el primario se considera fallido
- **AND** la respuesta la sirve el fallback
- **AND** `x-gateway-backend` identifica al fallback

#### Scenario: No-stream sin `timeout_ms` y sin fallback disponible
- **GIVEN** una ruta con un único backend sin `timeout_ms` que nunca responde
- **WHEN** llega una petición no-stream
- **THEN** al vencer `DEFAULT_REQUEST_TIMEOUT` el gateway responde `502` con error estilo OpenAI
- **AND** NO deja la petición colgada indefinidamente

#### Scenario: `timeout_ms` explícito tiene prioridad sobre el default
- **GIVEN** un backend que declara `timeout_ms` con un valor concreto
- **WHEN** llega una petición no-stream
- **THEN** se respeta ese `timeout_ms` y NO se aplica `DEFAULT_REQUEST_TIMEOUT`

#### Scenario: Stream sin `timeout_ms` acota solo el primer byte
- **GIVEN** un backend sin `timeout_ms` que tarda más que `DEFAULT_FIRST_BYTE_TIMEOUT` en emitir el primer byte
- **WHEN** llega una petición con `stream: true`
- **THEN** el gateway lo considera fallido antes del primer byte y prueba el siguiente backend de la cadena
