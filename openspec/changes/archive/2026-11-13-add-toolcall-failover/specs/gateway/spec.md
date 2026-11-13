## MODIFIED Requirements

### Requirement: Paso transparente del cuerpo y de los tool_calls

El gateway SHALL reenviar el cuerpo tal cual, sustituyendo únicamente el campo `model` por el del backend de destino. El gateway SHALL NOT modificar ni eliminar ningún otro campo de la petición, incluidos `tools` y `tool_choice`. El gateway MAY inspeccionar el cuerpo de la **respuesta** para decidir el failover, pero SHALL devolver al cliente los bytes del upstream sin alterarlos.

**Scenarios:**

#### Scenario: Preserva campos desconocidos
- **GIVEN** un cuerpo con `tools`, `tool_choice`, `temperature`, `response_format` y un campo futuro `x_custom`
- **WHEN** se reenvía
- **THEN** el backend recibe exactamente esos campos
- **AND** solo `model` cambió

#### Scenario: Preserva tool_calls en la respuesta
- **GIVEN** que el backend responde con `tool_calls`
- **WHEN** el gateway devuelve la respuesta
- **THEN** el cliente recibe los `tool_calls` intactos

#### Scenario: La inspección no altera la respuesta
- **GIVEN** una respuesta `200` cuyo cuerpo se inspecciona para decidir el failover
- **WHEN** el gateway la devuelve al cliente
- **THEN** el cuerpo es byte a byte el del upstream
- **AND** el `content-type` del upstream se preserva

## ADDED Requirements

### Requirement: Failover cuando la petición exige un tool call y la respuesta no lo trae

Cuando la petición incluya `tools` como lista no vacía y `tool_choice` exija una llamada a herramienta (`required`, `any` o un objeto que nombre una función), en modo **no-stream**, el gateway SHALL considerar intento fallido toda respuesta de éxito (`2xx`) cuyo cuerpo no contenga `tool_calls` en `choices[*].message`, y SHALL continuar con el siguiente backend de la cadena. Con `tool_choice` igual a `auto`, `none` o ausente, el gateway SHALL NOT aplicar esta condición. La condición SHALL NOT aplicarse en modo `stream: true`.

Si todos los backends de la cadena se agotan sin una respuesta con `tool_calls`, el gateway SHALL responder un error en formato OpenAI y SHALL NOT devolver la respuesta sin `tool_calls`.

**Scenarios:**

#### Scenario: Primario ignora las tools → failover
- **GIVEN** una ruta con primario y fallback, y una petición con `tools` y `tool_choice: "required"`
- **WHEN** el primario responde `200` sin `tool_calls` y el fallback responde `200` con `tool_calls`
- **THEN** el gateway devuelve la respuesta del fallback
- **AND** `x-gateway-backend` identifica el fallback

#### Scenario: tool_choice auto no dispara failover
- **GIVEN** una petición con `tools` y `tool_choice: "auto"`
- **WHEN** el primario responde `200` sin `tool_calls`
- **THEN** la respuesta del primario se devuelve tal cual
- **AND** NO se contacta el fallback

#### Scenario: tool_choice ausente no dispara failover
- **GIVEN** una petición con `tools` pero sin `tool_choice`
- **WHEN** el primario responde `200` sin `tool_calls`
- **THEN** la respuesta se devuelve y NO se contacta el fallback

#### Scenario: Función concreta exigida
- **GIVEN** una petición con `tool_choice` como objeto que nombra una función
- **WHEN** el primario responde `200` sin `tool_calls`
- **THEN** el gateway hace failover al siguiente backend

#### Scenario: Primario responde con tool_calls → sin failover
- **GIVEN** una petición con `tools` y `tool_choice: "required"`
- **WHEN** el primario responde `200` con `tool_calls`
- **THEN** la respuesta del primario se devuelve tal cual
- **AND** NO se contacta el fallback

#### Scenario: Todos sin tool call
- **GIVEN** todos los backends responden `200` sin `tool_calls`
- **WHEN** se agota la cadena
- **THEN** el gateway responde error estilo OpenAI con código `no_tool_call`
- **AND** NO devuelve ninguna respuesta sin `tool_calls`

#### Scenario: Streaming no aplica la condición
- **GIVEN** `stream: true` con `tools` y `tool_choice: "required"`
- **WHEN** el primario abre el stream
- **THEN** el gateway lo reenvía sin inspeccionar `tool_calls`
- **AND** NO hace failover por esta condición