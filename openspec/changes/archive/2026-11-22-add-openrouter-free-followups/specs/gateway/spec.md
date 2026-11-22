# Spec Delta

## MODIFIED Requirements

### Requirement: Configuración de rutas por YAML

El gateway SHALL configurarse con un fichero YAML (por defecto `gateway.yaml`, o la ruta indicada por `GATEWAY_CONFIG`) que define una lista de rutas; cada ruta tiene un `name` y una lista ordenada `backend`. Cada `Backend` SHALL tener un `kind` opcional (`static` por defecto u `openrouter_free`). Un `Backend` con `kind: static` SHALL requerir `base_url` y `model`. Un `Backend` con `kind: openrouter_free` SHALL requerir `base_url` y `api_key_env`, SHALL aceptar un `profile` opcional (`architect` | `code` | `writer`, default `code`) y un bloque `discovery` opcional. El bloque `Discovery` SHALL aplicar defaults por campo: `refresh_interval_ms` (default `300000`) y `blacklist_cooldown_ms` (default `60000`); `min_context` y `overrides` SHALL ser opcionales. Un bloque `discovery` con solo `min_context` SHALL ser válido. Los valores explícitos de `refresh_interval_ms` o `blacklist_cooldown_ms` iguales a `0` SHALL ser error de configuración que indique la ruta y el backend.

- `pub struct Discovery { pub refresh_interval_ms: u64, pub blacklist_cooldown_ms: u64, pub min_context: Option<u32>, pub overrides: Option<ClassifyOverrides> }`
- `refresh_interval_ms` SHALL deserializarse con `#[serde(default = "default_refresh_interval_ms")]`.
- `blacklist_cooldown_ms` SHALL deserializarse con `#[serde(default = "default_blacklist_cooldown_ms")]`.

**Scenarios:**

#### Scenario: Config válida
- **GIVEN** un `gateway.yaml` con una ruta `mech-writer` y dos backends
- **WHEN** se carga la config
- **THEN** la ruta existe con 2 backends en orden

#### Scenario: Ruta sin backends
- **GIVEN** una ruta con `backend: []`
- **WHEN** se valida
- **THEN** error de configuración indicando el nombre de la ruta

#### Scenario: Fichero ausente
- **GIVEN** que `GATEWAY_CONFIG` apunta a un fichero inexistente
- **WHEN** arranca
- **THEN** el proceso aborta con un error claro

#### Scenario: YAML inválido
- **GIVEN** un `gateway.yaml` con sintaxis YAML incorrecta
- **WHEN** se carga
- **THEN** el proceso aborta con un error que incluye la línea y la columna del problema

#### Scenario: Backend estático sin `kind`
- **GIVEN** un `gateway.yaml` con un backend sin `kind` y con `base_url` y `model`
- **WHEN** se carga la config
- **THEN** el backend se interpreta como `kind: static` y la ruta queda disponible

#### Scenario: Backend dinámico válido
- **GIVEN** un backend con `kind: openrouter_free`, `base_url`, `api_key_env` y `profile: code`
- **WHEN** se carga la config
- **THEN** el backend se acepta como dinámico con perfil `code`

#### Scenario: Backend estático sin `model`
- **GIVEN** un backend con `kind: static` sin `model`
- **WHEN** se valida
- **THEN** error de configuración indicando el nombre de la ruta y el backend

#### Scenario: Backend dinámico sin `api_key_env`
- **GIVEN** un backend con `kind: openrouter_free` sin `api_key_env`
- **WHEN** se valida
- **THEN** error de configuración indicando el backend afectado

#### Scenario: Perfil inválido
- **GIVEN** un backend con `kind: openrouter_free` y `profile: cualquiera`
- **WHEN** se valida
- **THEN** error de configuración listando los perfiles admitidos

#### Scenario: `discovery` con solo `min_context`
- **GIVEN** un backend `openrouter_free` con `discovery: { min_context: 32768 }`
- **WHEN** se carga la config
- **THEN** el bloque se acepta con `refresh_interval_ms` y `blacklist_cooldown_ms` en sus valores por defecto

#### Scenario: `discovery` vacío
- **GIVEN** un backend `openrouter_free` con `discovery: {}`
- **WHEN** se carga la config
- **THEN** el bloque se acepta con todos los campos en sus valores por defecto

#### Scenario: TTL cero explícito
- **GIVEN** un bloque `discovery` con `refresh_interval_ms: 0`
- **WHEN** se valida
- **THEN** error de configuración que indica ruta, backend y campo

### Requirement: Descubrimiento del catálogo free de OpenRouter

El gateway SHALL obtener el catálogo de modelos desde `GET {base_url}/models` sin autenticación, SHALL filtrar a modelos gratuitos (identificador con sufijo `:free` o con `pricing.prompt` y `pricing.completion` iguales a `0`) que además expongan tool-calling (`supported_parameters` contiene `tools` y `tool_choice`), y SHALL almacenarlo en una caché con TTL por `base_url`. El TTL de cada `base_url` SHALL ser el `refresh_interval_ms` común de todos los backends dinámicos que la comparten; si dos backends dinámicos declaran TTLs distintos para la misma `base_url`, la validación de configuración SHALL fallar con un error que nombre la `base_url` y las rutas implicadas. Si un backend dinámico no declara `discovery`, SHALL usar el TTL por defecto. Un catálogo vacío `{"data": []}` SHALL ser válido y producir cero modelos elegibles, sin error. El refresco SHALL ejecutarse en segundo plano con `stale-while-error`.

**Scenarios:**

#### Scenario: Descubrimiento inicial
- **GIVEN** un backend `openrouter_free` sin caché previa
- **WHEN** llega la primera petición
- **THEN** el gateway consulta `/models`, puebla la caché y resuelve la ruta con los modelos elegibles

#### Scenario: Exclusión de modelos de pago
- **GIVEN** un catálogo que contiene modelos con pricing no nulo
- **WHEN** se filtra
- **THEN** esos modelos no aparecen como elegibles

#### Scenario: Exclusión de modelos sin tool-calling
- **GIVEN** un modelo gratuito cuyo `supported_parameters` no contiene `tools` ni `tool_choice`
- **WHEN** se filtra
- **THEN** ese modelo no aparece como elegible

#### Scenario: Fallo de red con caché válida
- **GIVEN** una caché poblada y un fallo al refrescar
- **WHEN** se intenta el refresco
- **THEN** se conserva la última caché válida y las peticiones siguen resolviéndose

#### Scenario: TTL vencido en segundo plano
- **GIVEN** una caché cuyo TTL ha vencido
- **WHEN** llega una petición
- **THEN** el gateway responde con la caché existente y dispara el refresco en segundo plano sin bloquear la petición

#### Scenario: Catálogo vacío
- **GIVEN** `GET /models` devuelve `{"data": []}`
- **WHEN** se consulta el catálogo
- **THEN** la caché queda vacía, no se produce error y la ruta dinámica no aporta candidatos

#### Scenario: TTL vencido combinado con fallo de red
- **GIVEN** una caché con TTL vencido y una petición de refresco que falla
- **WHEN** se solicita la expansión de la ruta dinámica
- **THEN** se usan los modelos de la caché previa si existe, o cero candidatos si nunca hubo catálogo, sin pánico ni error propagado

#### Scenario: TTL incoherente por `base_url`
- **GIVEN** dos backends dinámicos con la misma `base_url` y `refresh_interval_ms` distintos
- **WHEN** se valida la config
- **THEN** error de configuración que indica la `base_url` y las rutas afectadas

#### Scenario: TTL coherente por `base_url`
- **GIVEN** dos backends dinámicos con la misma `base_url` y el mismo `refresh_interval_ms`
- **WHEN** se valida la config
- **THEN** la config se acepta y ambos comparten un único catálogo con ese TTL

### Requirement: Clasificación de modelos por perfil

El gateway SHALL clasificar cada modelo free elegible en los perfiles `architect`, `code` y `writer` mediante una heurística sobre `id`, `name`, `description` y `context_length`, SHALL ordenar cada perfil por tier, después por `context_length` descendente y, a igualdad, por `id` ascendente, y SHALL permitir reclasificaciones manuales por configuración (`exclude`/`include`). Un `include` que nombra un modelo no elegible (ausente del catálogo, filtrado por `min_context` o por `exclude`) SHALL ignorarse en la lista resultante y SHALL emitir un `tracing::warn!` que incluya el identificador del modelo, el perfil y la `base_url`. La clasificación no SHALL cambiar su salida por efecto del aviso.

**Scenarios:**

#### Scenario: `architect` prioriza razonamiento
- **GIVEN** un catálogo con un modelo de la familia de razonamiento (p. ej. `deepseek-r1`, `qwq`, `o3`, `reasoner`) y varios generales
- **WHEN** se clasifica el perfil `architect`
- **THEN** el modelo de razonamiento queda en el primer lugar

#### Scenario: `architect` cae a general grande
- **GIVEN** un catálogo sin modelos de razonamiento elegibles pero con un general grande (p. ej. `llama-3.3-70b`, `qwen-*72b`, `deepseek-v3`)
- **WHEN** se clasifica el perfil `architect`
- **THEN** el general grande queda como primer candidato

#### Scenario: `code` prioriza familias coder
- **GIVEN** un catálogo con un modelo de familia coder (p. ej. `coder`, `codestral`, `devstral`) y varios generales
- **WHEN** se clasifica el perfil `code`
- **THEN** el modelo coder queda en el primer lugar

#### Scenario: `writer` prioriza instruct general
- **GIVEN** un catálogo con modelos coder y generales
- **WHEN** se clasifica el perfil `writer`
- **THEN** los modelos generales instruct preceden a los exclusivamente coder

#### Scenario: Orden determinista
- **GIVEN** dos modelos elegibles con distinto `context_length`
- **WHEN** se clasifica un perfil
- **THEN** el de mayor `context_length` precede al de menor

#### Scenario: `min_context` descarta modelos pequeños
- **GIVEN** `discovery.min_context` configurado y un modelo elegible con `context_length` inferior
- **WHEN** se clasifica
- **THEN** ese modelo no aparece en ningún perfil

#### Scenario: Override manual
- **GIVEN** un override de configuración que excluye un modelo del perfil `code`
- **WHEN** se clasifica el perfil `code`
- **THEN** ese modelo no aparece en la lista resultante

#### Scenario: `include` antepone un modelo elegible
- **GIVEN** un `include` con un modelo presente y elegible
- **WHEN** se clasifica
- **THEN** ese modelo aparece el primero y no se duplica

#### Scenario: `include` no elegible avisa y se descarta
- **GIVEN** un `include` con un modelo ausente del catálogo o filtrado por `min_context`
- **WHEN** se clasifica
- **THEN** el modelo no aparece en la lista y se emite un warning con su identificador, el perfil y la `base_url`

### Requirement: Expansión y rotación de backends dinámicos

En cada petición, el gateway SHALL expandir cada `Backend` de tipo `openrouter_free` a la lista de modelos concretos clasificados para su perfil, en la misma posición que ocupa dentro de la ruta, SHALL excluir los modelos en blacklist y SHALL aplicar rotación round-robin entre peticiones sucesivas, de modo que el orden de expansión avance en cada petición. El candidato resultante SHALL ser una estructura ligera que contenga únicamente el índice de slot, `model`, `base_url`, `timeout_ms`, `api_key_env` y el cooldown de blacklist aplicable; la expansión no SHALL clonar el `Backend` completo (ni `discovery`, ni `overrides`, ni `warmup`). La resolución del modelo, la cabecera de autenticación, el timeout y el marcado en blacklist SHALL producir exactamente los mismos valores que antes del cambio.

**Scenarios:**

#### Scenario: Expansión en posición
- **GIVEN** una ruta con un backend dinámico entre dos backends estáticos
- **WHEN** se resuelve la ruta
- **THEN** los modelos expandidos aparecen entre el primero y el último backend estático

#### Scenario: Rotación round-robin
- **GIVEN** una ruta con un backend dinámico y varios modelos elegibles
- **WHEN** llegan peticiones sucesivas
- **THEN** el primer modelo de la lista rota de forma round-robin entre peticiones

#### Scenario: Exclusión de blacklist
- **GIVEN** un modelo marcado en blacklist
- **WHEN** se expande el backend dinámico
- **THEN** ese modelo no aparece entre los candidatos

#### Scenario: Perfil sin modelos elegibles
- **GIVEN** un perfil sin ningún modelo elegible tras el filtrado
- **WHEN** se resuelve la ruta
- **THEN** el backend dinámico no aporta candidatos y la ruta continúa con el siguiente backend

#### Scenario: Candidato ligero sin clon del backend
- **GIVEN** una ruta con un backend dinámico con `discovery.overrides` y `warmup` declarados
- **WHEN** se expande la ruta
- **THEN** cada candidato expone solo `model`, `base_url`, `timeout_ms` y `api_key_env` (más slot y cooldown) y la petición se envía con el `model` expandido y la `api_key_env` del backend original

#### Scenario: Blacklist compartida entre rutas
- **GIVEN** dos rutas con la misma `base_url` y perfiles distintos
- **WHEN** un modelo se marca en blacklist desde la primera ruta
- **THEN** la segunda ruta tampoco lo propone mientras dure el cooldown

#### Scenario: `/v1/models` deduplica sin duplicados
- **GIVEN** dos backends dinámicos con perfiles distintos que producen modelos solapados
- **WHEN** se consulta `GET /v1/models`
- **THEN** cada identificador aparece una sola vez

### Requirement: Métricas de descubrimiento

El gateway SHALL exponer métricas con el prefijo `llm_gateway_discovery_` que incluyan, al menos, el número de modelos del catálogo, el número de modelos free elegibles, el número de elegibles por perfil, el resultado de los refrescos y el tamaño de la blacklist, integradas en la exportación OTLP/Prometheus existente. El gauge `llm_gateway_discovery_blacklist` SHALL reflejar el número de modelos en blacklist tras purgar los cooldowns vencidos en cada acceso, y su escritura SHALL omitirse cuando el tamaño no ha cambiado desde la última escritura, sin alterar el valor observable.

**Scenarios:**

#### Scenario: Métricas tras descubrimiento
- **GIVEN** un descubrimiento completado
- **WHEN** se consultan las métricas
- **THEN** aparecen el total de modelos y los elegibles por perfil

#### Scenario: Fallo de refresco
- **GIVEN** un refresco fallido
- **WHEN** se consultan las métricas
- **THEN** el contador de fallos de refresco aumenta

#### Scenario: Tamaño de blacklist
- **GIVEN** uno o más modelos en blacklist
- **WHEN** se consultan las métricas
- **THEN** el tamaño de la blacklist refleja el número de modelos excluidos

#### Scenario: Gauge refleja una marca
- **GIVEN** una blacklist vacía
- **WHEN** se marca un modelo
- **THEN** el gauge vale 1

#### Scenario: Gauge vuelve a cero al expirar
- **GIVEN** un modelo marcado con cooldown corto
- **WHEN** vence el cooldown y se consulta la blacklist
- **THEN** el gauge vale 0

#### Scenario: Escritura solo ante cambios
- **GIVEN** una blacklist con un modelo marcado
- **WHEN** se realizan accesos sucesivos sin cambios de tamaño
- **THEN** el gauge conserva el mismo valor y no se reescribe en cada acceso