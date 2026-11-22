# Spec Delta

## MODIFIED Requirements

### Requirement: Configuración de rutas por YAML

El gateway SHALL configurarse con un fichero YAML (por defecto `gateway.yaml`, o la ruta indicada por `GATEWAY_CONFIG`) que define una lista de rutas; cada ruta tiene un `name` (nombre lógico que pide el cliente) y una lista **ordenada** `backend` (primario primero, respaldos después). Cada `Backend` SHALL tener un `kind` opcional: `static` (por defecto) u `openrouter_free`. Un `Backend` con `kind: static` SHALL requerir `base_url` y `model`. Un `Backend` con `kind: openrouter_free` SHALL requerir `base_url` y `api_key_env`, SHALL aceptar un `profile` opcional (`architect` | `code` | `writer`, default `code`) y un bloque `discovery` opcional, y SHALL ignorar el `model` si se declara. El parseo se hace con `serde-saphyr` y la validación semántica SHALL rechazar configuraciones inválidas con un error que indique la ruta y el backend afectados.

- `pub struct Backend { pub kind: BackendKind, pub base_url: String, pub model: Option<String>, pub api_key_env: Option<String>, pub timeout_ms: Option<u64>, pub profile: Option<Profile>, pub discovery: Option<Discovery>, pub warmup: Option<Warmup> }`
- `pub enum BackendKind { Static, OpenrouterFree }`
- `pub enum Profile { Architect, Code, Writer }`
- `pub struct Discovery { pub refresh_interval_ms: u64, pub blacklist_cooldown_ms: u64, pub min_context: Option<u32> }`

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

## ADDED Requirements

### Requirement: Descubrimiento del catálogo free de OpenRouter

El gateway SHALL obtener el catálogo de modelos desde `GET {base_url}/models` sin autenticación, SHALL filtrar a modelos gratuitos (identificador con sufijo `:free` o con `pricing.prompt` y `pricing.completion` iguales a `0`) que además expongan tool-calling (`supported_parameters` contiene `tools` y `tool_choice`), y SHALL almacenarlo en una caché con TTL igual a `refresh_interval_ms`.

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

### Requirement: Clasificación de modelos por perfil

El gateway SHALL clasificar cada modelo free elegible en los perfiles `architect`, `code` y `writer` mediante una heurística sobre `id`, `name`, `description` y `context_length`, SHALL ordenar cada perfil por `context_length` descendente y, a igualdad, por `id` ascendente, y SHALL permitir reclasificaciones manuales por configuración.

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

### Requirement: Expansión y rotación de backends dinámicos

En cada petición, el gateway SHALL expandir cada `Backend` de tipo `openrouter_free` a la lista de modelos concretos clasificados para su perfil, en la misma posición que ocupa dentro de la ruta, SHALL excluir los modelos en blacklist y SHALL aplicar rotación round-robin entre peticiones sucesivas, de modo que el orden de expansión avance en cada petición.

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

### Requirement: Blacklist dinámica de modelos

El gateway SHALL marcar un modelo como no elegible durante `blacklist_cooldown_ms` cuando una petición dirigida a ese modelo devuelva `429`, un código `5xx` o un encabezado `Retry-After`, y SHALL compartir esa blacklist entre todas las rutas.

**Scenarios:**

#### Scenario: 429 marca el modelo
- **GIVEN** una petición a un modelo que responde `429`
- **WHEN** se procesa la respuesta
- **THEN** el modelo entra en blacklist durante `blacklist_cooldown_ms`

#### Scenario: 5xx marca el modelo
- **GIVEN** una petición a un modelo que responde `503`
- **WHEN** se procesa la respuesta
- **THEN** el modelo entra en blacklist

#### Scenario: Retry-After respetado
- **GIVEN** una respuesta con encabezado `Retry-After`
- **WHEN** se calcula el cooldown
- **THEN** el modelo queda en blacklist al menos durante el tiempo indicado

#### Scenario: Expiración del cooldown
- **GIVEN** un modelo en blacklist cuyo cooldown ha vencido
- **WHEN** se expande un backend dinámico
- **THEN** ese modelo vuelve a ser elegible

#### Scenario: Blacklist compartida
- **GIVEN** un modelo marcado en blacklist por una ruta
- **WHEN** otra ruta intenta usar el mismo modelo
- **THEN** el modelo sigue excluido

### Requirement: Fallback y agotamiento de ruta

El gateway SHALL continuar al siguiente backend de la ruta cuando un backend dinámico no aporte candidatos o todos sus modelos fallen, SHALL usar los backends de pago declarados como respaldo y, si se agota la ruta completa, SHALL responder con un error `AllBackendsFailed`.

**Scenarios:**

#### Scenario: Free agotados, fallback de pago
- **GIVEN** una ruta con un backend dinámico y un backend de pago posterior, y todos los modelos free en blacklist o fallando
- **WHEN** se procesa la petición
- **THEN** la petición se sirve con el backend de pago

#### Scenario: Ruta sin backends utilizables
- **GIVEN** una ruta cuyos backends dinámicos no aportan candidatos y sin backends estáticos disponibles
- **WHEN** se procesa la petición
- **THEN** el gateway responde `AllBackendsFailed`

#### Scenario: Failover antes del primer token
- **GIVEN** un modelo free que falla antes de emitir el primer token
- **WHEN** se procesa la petición
- **THEN** el gateway prueba el siguiente candidato de la ruta sin haber enviado nada al cliente

### Requirement: Métricas de descubrimiento

El gateway SHALL exponer métricas con el prefijo `llm_gateway_discovery_` que incluyan, al menos, el número de modelos del catálogo, el número de modelos free elegibles, el número de elegibles por perfil, el resultado de los refrescos y el tamaño de la blacklist, integradas en la exportación OTLP/Prometheus existente.

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
