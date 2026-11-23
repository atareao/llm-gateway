//! Carga y validación de la configuración de rutas del gateway (YAML).

use std::collections::{HashMap, hash_map::Entry};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// Variable de entorno que sobrescribe la ruta del fichero de configuración.
pub const GATEWAY_CONFIG_ENV: &str = "GATEWAY_CONFIG";

/// Ruta por defecto del fichero de configuración.
pub const DEFAULT_CONFIG_PATH: &str = "gateway.yaml";

/// Intervalo de refresco por defecto del catálogo dinámico (5 min).
pub const DEFAULT_REFRESH_INTERVAL_MS: u64 = 300_000;

/// Cooldown por defecto de la blacklist dinámica (1 min).
pub const DEFAULT_BLACKLIST_COOLDOWN_MS: u64 = 60_000;

/// Intervalo por defecto del sondeo de la cuota free (1 min).
pub const DEFAULT_QUOTA_POLL_INTERVAL_MS: u64 = 60_000;

/// Cadencia por defecto de reselección del modelo de pago (4 h).
pub const DEFAULT_RESELECT_INTERVAL_MS: u64 = 4 * 60 * 60 * 1_000;

/// Mejora relativa por defecto exigida para cambiar de modelo de pago (20 %).
pub const DEFAULT_MIN_PRICE_DELTA: f64 = 0.20;

/// Configuración completa del gateway.
#[derive(Debug, Deserialize)]
pub struct GatewayConfig {
    /// Rutas lógicas que el cliente puede pedir como `model`.
    pub route: Vec<Route>,
    /// Bloque opcional de observabilidad (exportación OTLP).
    pub observability: Option<Observability>,
}

impl GatewayConfig {
    /// ¿Emite el gateway los eventos del ledger (`catalog`, `selection`,
    /// `request`)? Resuelve el default del bloque ausente o del campo
    /// `events` ausente: siempre `true` salvo opt-out explícito.
    pub fn ledger_events_enabled(&self) -> bool {
        self.observability
            .as_ref()
            .is_none_or(|observability| observability.events)
    }
}

/// Configuración de observabilidad. Ausente en el YAML → `None` y el gateway
/// no inicializa ningún exportador.
#[derive(Debug, Deserialize)]
pub struct Observability {
    /// Emisión de los eventos del ledger (`catalog`, `selection`, `request`)
    /// como logs estructurados por `tracing`. Por defecto `true`;
    /// `false` desactiva los tres eventos **sin** afectar a las métricas
    /// Prometheus ni a la exportación OTLP. Ver `docs/observability-ledger.md`.
    #[serde(default = "default_events")]
    pub events: bool,
    /// Exportación OTLP opcional.
    pub otlp: Option<Otlp>,
}

/// Valor por defecto de `observability.events` (`true`): los eventos se emiten
/// salvo opt-out explícito.
fn default_events() -> bool {
    true
}

/// Exportación OTLP hacia un colector (p. ej. OpenObserve).
#[derive(Debug, Deserialize)]
pub struct Otlp {
    /// Base del colector, sin el sufijo `/api/<organización>`. El gateway
    /// compone `{endpoint}/api/{organization}/v1/{metrics|logs|traces}`.
    pub endpoint: String,
    /// Organización destino del colector.
    pub organization: String,
    /// Protocolo de transporte; por defecto [`Protocol::Http`].
    #[serde(default)]
    pub protocol: Protocol,
    /// Nombre de la variable de entorno con el token de ingesta (opcional).
    /// El token se envía como `Authorization: Basic <token>`.
    pub token_env: Option<String>,
    /// Nombre de servicio reportado como recurso OTel (opcional).
    pub service_name: Option<String>,
    /// Intervalo de exportación de métricas y logs, en milisegundos
    /// (opcional; por defecto `telemetry::DEFAULT_OTLP_INTERVAL_MS`).
    pub interval_ms: Option<u64>,
    /// Timeout de cada exportación, en milisegundos (opcional).
    pub timeout_ms: Option<u64>,
    /// Señales a exportar; al menos una debe estar activa.
    pub signals: Signals,
}

/// Protocolo de transporte OTLP.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    /// OTLP sobre HTTP con codificación protobuf.
    #[default]
    Http,
    /// OTLP sobre gRPC. **Aún no soportado**: se parsea para poder dar un error
    /// de configuración explícito (ver `validate_otlp`).
    Grpc,
}

/// Señales de telemetría a exportar. Cada campo ausente queda a `false`.
#[derive(Debug, Default, Deserialize)]
pub struct Signals {
    /// Exportar métricas por OTLP.
    #[serde(default)]
    pub metrics: bool,
    /// Exportar logs por OTLP.
    #[serde(default)]
    pub logs: bool,
    /// Exportar trazas por OTLP.
    #[serde(default)]
    pub traces: bool,
}

impl Signals {
    /// `true` si al menos una señal está activa.
    pub fn any(&self) -> bool {
        self.metrics || self.logs || self.traces
    }
}

/// Ruta lógica: un nombre y una cadena ordenada de backends (primario primero).
#[derive(Debug, Deserialize)]
pub struct Route {
    /// Nombre lógico que pide el cliente (p. ej. `mech-writer`).
    pub name: String,
    /// Backends en orden: el primero es el primario, el resto son respaldos.
    pub backend: Vec<Backend>,
}

/// Backend concreto al que se reenvía la petición.
///
/// El campo `kind` distingue un backend **estático** (por defecto) de uno
/// **dinámico** (`openrouter_free`), que se expande a los modelos concretos del
/// catálogo free clasificados para su [`Profile`].
#[derive(Debug, Clone, Deserialize)]
pub struct Backend {
    /// Tipo de backend; por defecto [`BackendKind::Static`] (compatibilidad con
    /// las configuraciones existentes que no declaran `kind`).
    #[serde(default)]
    pub kind: BackendKind,
    /// URL base de la API compatible con OpenAI (sin `/chat/completions`).
    pub base_url: String,
    /// Modelo real que entiende este backend. Requerido para `kind: static`;
    /// ignorado para `kind: openrouter_free`.
    #[serde(default)]
    pub model: Option<String>,
    /// Nombre de la variable de entorno que contiene la API key (opcional).
    pub api_key_env: Option<String>,
    /// Timeout de la petición en milisegundos (opcional).
    pub timeout_ms: Option<u64>,
    /// Perfil de clasificación para un backend `openrouter_free` (opcional;
    /// por defecto [`Profile::Code`]). Se ignora en backends estáticos.
    #[serde(default)]
    pub profile: Option<Profile>,
    /// Bloque opcional de descubrimiento para un backend `openrouter_free`.
    pub discovery: Option<Discovery>,
    /// Bloque opcional de keep-alive (warmup) para este backend.
    pub warmup: Option<Warmup>,
}

/// Tipo de backend de la ruta.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    /// Backend fijo con `model` explícito (comportamiento histórico).
    #[default]
    Static,
    /// Backend dinámico que se expande al catálogo free de OpenRouter.
    OpenrouterFree,
    /// Backend dinámico que se expande a la lista curada de modelos de pago.
    OpenrouterPaid,
}

impl BackendKind {
    /// Etiqueta estable del tipo de backend, usada en los mensajes de configuración.
    pub fn label(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::OpenrouterFree => "openrouter_free",
            Self::OpenrouterPaid => "openrouter_paid",
        }
    }
}

/// Perfil de uso que determina la clasificación de modelos dinámicos.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    /// Prioriza familias de razonamiento y, en su defecto, generales grandes.
    Architect,
    /// Prioriza familias orientadas a código.
    #[default]
    Code,
    /// Prioriza modelos generales instruct frente a los exclusivamente coder.
    Writer,
}

impl Profile {
    /// Etiqueta estable del perfil, usada en métricas y logs.
    pub fn label(self) -> &'static str {
        match self {
            Self::Architect => "architect",
            Self::Code => "code",
            Self::Writer => "writer",
        }
    }
}

/// Valor por defecto de `refresh_interval_ms` ([`DEFAULT_REFRESH_INTERVAL_MS`]).
fn default_refresh_interval_ms() -> u64 {
    DEFAULT_REFRESH_INTERVAL_MS
}

/// Valor por defecto de `blacklist_cooldown_ms` ([`DEFAULT_BLACKLIST_COOLDOWN_MS`]).
fn default_blacklist_cooldown_ms() -> u64 {
    DEFAULT_BLACKLIST_COOLDOWN_MS
}

/// Valor por defecto de `respect_daily_quota` (`true`).
fn default_respect_daily_quota() -> bool {
    true
}

/// Valor por defecto de `quota_poll_interval_ms` ([`DEFAULT_QUOTA_POLL_INTERVAL_MS`]).
fn default_quota_poll_interval_ms() -> u64 {
    DEFAULT_QUOTA_POLL_INTERVAL_MS
}

/// Valor por defecto de `reselect_interval_ms` ([`DEFAULT_RESELECT_INTERVAL_MS`]).
fn default_reselect_interval_ms() -> u64 {
    DEFAULT_RESELECT_INTERVAL_MS
}

/// Valor por defecto de `min_price_delta` ([`DEFAULT_MIN_PRICE_DELTA`]).
fn default_min_price_delta() -> f64 {
    DEFAULT_MIN_PRICE_DELTA
}

/// Configuración de descubrimiento de un backend `openrouter_free`.
#[derive(Debug, Clone, Deserialize)]
pub struct Discovery {
    /// TTL de la caché del catálogo y periodo de refresco en segundo plano.
    #[serde(default = "default_refresh_interval_ms")]
    pub refresh_interval_ms: u64,
    /// Tiempo que un modelo permanece en la blacklist tras un fallo.
    #[serde(default = "default_blacklist_cooldown_ms")]
    pub blacklist_cooldown_ms: u64,
    /// Contexto mínimo (en tokens) exigido a los modelos elegibles.
    pub min_context: Option<u32>,
    /// Reclasificaciones manuales por perfil (opcional).
    #[serde(default)]
    pub overrides: Option<ClassifyOverrides>,
    /// Respetar la cuota diaria free: abre el gate global al detectar el
    /// agotamiento y sondea `GET {base_url}/key` para anticiparlo. Por defecto
    /// `true`; `false` desactiva gate y sondeo para este backend.
    #[serde(default = "default_respect_daily_quota")]
    pub respect_daily_quota: bool,
    /// Intervalo del sondeo de cuota, en milisegundos (debe ser mayor que 0).
    #[serde(default = "default_quota_poll_interval_ms")]
    pub quota_poll_interval_ms: u64,
    /// Lista curada de patrones glob (`*` = cero o más caracteres) sobre el
    /// `model_id` para un backend `openrouter_paid`. Sin lista por defecto en el
    /// código: ausente o vacía ⇒ el backend de pago aporta cero candidatos; la
    /// curación vive en la configuración.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Cadencia mínima de reselección del modelo de pago, en milisegundos.
    #[serde(default = "default_reselect_interval_ms")]
    pub reselect_interval_ms: u64,
    /// Mejora relativa mínima de precio exigida para cambiar de modelo de pago.
    #[serde(default = "default_min_price_delta")]
    pub min_price_delta: f64,
}

/// Reclasificaciones manuales aplicadas a la heurística de clasificación.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ClassifyOverrides {
    /// Identificadores de modelo excluidos de este perfil.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Identificadores de modelo forzados al principio de este perfil.
    #[serde(default)]
    pub include: Vec<String>,
}

impl Discovery {
    /// Intervalo de refresco efectivo del bloque o el valor por defecto.
    pub fn refresh_interval(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.refresh_interval_ms)
    }

    /// Cooldown efectivo de la blacklist del bloque o el valor por defecto.
    pub fn blacklist_cooldown(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.blacklist_cooldown_ms)
    }

    /// Intervalo efectivo del sondeo de cuota del bloque.
    pub fn quota_poll_interval(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.quota_poll_interval_ms)
    }

    /// Cadencia efectiva de reselección del modelo de pago del bloque.
    pub fn reselect_interval(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.reselect_interval_ms)
    }
}

impl Backend {
    /// `true` si el backend es dinámico (`openrouter_free` u `openrouter_paid`).
    pub fn is_dynamic(&self) -> bool {
        matches!(
            self.kind,
            BackendKind::OpenrouterFree | BackendKind::OpenrouterPaid
        )
    }

    /// `true` si el backend pertenece al tier free (`openrouter_free`): solo esos
    /// abren el gate global de cuota diaria.
    pub fn is_free_tier(&self) -> bool {
        self.kind == BackendKind::OpenrouterFree
    }

    /// Perfil efectivo del backend dinámico (por defecto [`Profile::Code`]).
    pub fn profile(&self) -> Profile {
        self.profile.unwrap_or_default()
    }

    /// Etiqueta legible del backend para mensajes de error, en el formato
    /// `<base_url>#<model|dynamic>`.
    pub fn label(&self) -> String {
        format_id(&self.base_url, self.model.as_deref().unwrap_or("dynamic"))
    }
}

/// Identificador determinista de un backend, en el formato `<base_url>#<model>`.
///
/// Es el único punto que produce la etiqueta que comparten [`Backend::label`],
/// `proxy::backend_id` y el `Candidate` expandido, para que las tres no diverjan.
pub(crate) fn format_id(base_url: &str, model: &str) -> String {
    format!("{base_url}#{model}")
}

/// Configuración de keep-alive (warmup) de un backend.
#[derive(Debug, Clone, Deserialize)]
pub struct Warmup {
    /// Intervalo entre warmups, en milisegundos (debe ser mayor que 0).
    pub interval_ms: u64,
    /// Timeout de cada warmup, en milisegundos (opcional; si se omite se usa
    /// [`crate::warmup::WARMUP_TIMEOUT`], 30 s). Si está presente, debe ser
    /// mayor que 0.
    pub timeout_ms: Option<u64>,
}

/// Carga la configuración desde `path` y la valida.
///
/// # Errors
/// Devuelve error si el fichero no existe, si el YAML es inválido (el mensaje
/// incluye línea y columna) o si la validación semántica falla (p. ej. una
/// ruta sin backends).
pub fn load(path: &Path) -> Result<GatewayConfig> {
    let raw = std::fs::read_to_string(path).with_context(|| {
        format!(
            "config: no se pudo leer el fichero de configuración '{}'",
            path.display()
        )
    })?;

    let config: GatewayConfig =
        serde_saphyr::from_str(&raw).map_err(|err| yaml_error(err, path))?;

    validate(&config)?;
    Ok(config)
}

/// Convierte un error de `serde-saphyr` en un `anyhow::Error` con contexto y,
/// si el renderizado no trae ya la posición, la añade a partir de
/// [`serde_saphyr::Error::location`].
fn yaml_error(err: serde_saphyr::Error, path: &Path) -> anyhow::Error {
    let rendered = err.to_string();
    let location = err.location();

    let mut msg = format!("config: YAML inválido en '{}': {rendered}", path.display());
    if !rendered.contains("line")
        && let Some(loc) = location
    {
        msg.push_str(&format!(
            " (línea {}, columna {})",
            loc.line(),
            loc.column()
        ));
    }
    anyhow::anyhow!(msg)
}

/// Valida las invariantes semánticas que el deserializador no puede comprobar
/// por sí solo: cada ruta debe declarar al menos un backend, el TTL de
/// descubrimiento debe ser coherente por `base_url`, y el bloque opcional
/// `observability.otlp` debe ser coherente.
fn validate(config: &GatewayConfig) -> Result<()> {
    // D5: el catálogo de descubrimiento es único por `base_url`, así que todos
    // los backends dinámicos que la comparten deben refrescarlo con el mismo
    // TTL efectivo. Se conserva el primero visto para que el error determinista
    // pueda informar del TTL esperado.
    let mut dynamic_ttls: HashMap<&str, (u64, Vec<&str>)> = HashMap::new();

    for route in &config.route {
        if route.backend.is_empty() {
            bail!("config: la ruta '{}' no tiene backends", route.name);
        }
        for backend in &route.backend {
            match backend.kind {
                BackendKind::Static => {
                    if backend.model.as_deref().is_none_or(str::is_empty) {
                        bail!(
                            "config: la ruta '{}' backend '{}' es de tipo static y requiere 'model'",
                            route.name,
                            backend.label()
                        );
                    }
                }
                BackendKind::OpenrouterFree | BackendKind::OpenrouterPaid => {
                    if backend.api_key_env.as_deref().is_none_or(str::is_empty) {
                        bail!(
                            "config: la ruta '{}' backend '{}' es de tipo {} y requiere 'api_key_env'",
                            route.name,
                            backend.label(),
                            backend.kind.label()
                        );
                    }
                    if let Some(discovery) = &backend.discovery {
                        if discovery.refresh_interval_ms == 0 {
                            bail!(
                                "config: la ruta '{}' backend '{}' tiene discovery.refresh_interval_ms = 0; debe ser mayor que 0",
                                route.name,
                                backend.label()
                            );
                        }
                        if discovery.blacklist_cooldown_ms == 0 {
                            bail!(
                                "config: la ruta '{}' backend '{}' tiene discovery.blacklist_cooldown_ms = 0; debe ser mayor que 0",
                                route.name,
                                backend.label()
                            );
                        }
                        if discovery.quota_poll_interval_ms == 0 {
                            bail!(
                                "config: la ruta '{}' backend '{}' tiene discovery.quota_poll_interval_ms = 0; debe ser mayor que 0",
                                route.name,
                                backend.label()
                            );
                        }
                        if discovery.min_price_delta < 0.0 || !discovery.min_price_delta.is_finite()
                        {
                            bail!(
                                "config: la ruta '{}' backend '{}' tiene discovery.min_price_delta = {}; debe ser mayor o igual que 0",
                                route.name,
                                backend.label(),
                                discovery.min_price_delta
                            );
                        }
                        if let Some(position) = discovery
                            .allow
                            .iter()
                            .position(|pattern| pattern.trim().is_empty())
                        {
                            bail!(
                                "config: la ruta '{}' backend '{}' tiene discovery.allow con un patrón vacío en la posición {position}",
                                route.name,
                                backend.label()
                            );
                        }
                    }

                    let effective_ttl = backend
                        .discovery
                        .as_ref()
                        .map_or(DEFAULT_REFRESH_INTERVAL_MS, |discovery| {
                            discovery.refresh_interval_ms
                        });
                    match dynamic_ttls.entry(backend.base_url.as_str()) {
                        Entry::Vacant(slot) => {
                            slot.insert((effective_ttl, vec![route.name.as_str()]));
                        }
                        Entry::Occupied(mut slot) => {
                            let (expected_ttl, routes) = slot.get_mut();
                            if !routes.contains(&route.name.as_str()) {
                                routes.push(route.name.as_str());
                            }
                            if *expected_ttl != effective_ttl {
                                return Err(ttl_conflict_error(
                                    &backend.base_url,
                                    *expected_ttl,
                                    effective_ttl,
                                    routes,
                                ));
                            }
                        }
                    }
                }
            }

            if let Some(warmup) = &backend.warmup {
                if warmup.interval_ms == 0 {
                    bail!(
                        "config: la ruta '{}' backend '{}' tiene warmup.interval_ms = 0; debe ser mayor que 0",
                        route.name,
                        backend.label()
                    );
                }
                if warmup.timeout_ms == Some(0) {
                    bail!(
                        "config: la ruta '{}' backend '{}' tiene warmup.timeout_ms = 0; debe ser mayor que 0",
                        route.name,
                        backend.label()
                    );
                }
            }
        }
    }

    if let Some(observability) = &config.observability
        && let Some(otlp) = &observability.otlp
    {
        validate_otlp(otlp)?;
    }

    Ok(())
}

/// Construye el error de TTL de descubrimiento incoherente para una `base_url`
/// compartida por varias rutas.
///
/// Mismo formato de una línea que el resto de errores de `config:`. Es
/// determinista: `expected_ms` es el TTL del primer backend visto en orden de
/// YAML y `routes` conserva ese mismo orden.
fn ttl_conflict_error(
    base_url: &str,
    expected_ms: u64,
    found_ms: u64,
    routes: &[&str],
) -> anyhow::Error {
    let routes = routes
        .iter()
        .map(|name| format!("'{name}'"))
        .collect::<Vec<_>>()
        .join(", ");
    anyhow::anyhow!(
        "config: la base_url '{base_url}' se comparte entre las rutas [{routes}] con discovery.refresh_interval_ms distinto: se esperaba {expected_ms} ms y se encontró {found_ms} ms; unifica el TTL en todas las rutas que comparten esa base_url"
    )
}

/// Valida el bloque `observability.otlp`: endpoint y organización no vacíos,
/// intervalos positivos, al menos una señal activa y, si `token_env` está
/// definido, que la variable de entorno exista y no esté vacía.
fn validate_otlp(otlp: &Otlp) -> Result<()> {
    let endpoint = otlp.endpoint.trim();
    if endpoint.is_empty() {
        bail!("config: observability.otlp.endpoint no puede estar vacío");
    }
    // Sin esquema, el exportador OTLP/HTTP no podría construir la URL de envío.
    if !(endpoint.starts_with("http://") || endpoint.starts_with("https://")) {
        bail!(
            "config: observability.otlp.endpoint debe empezar por http:// o https://, got '{endpoint}'"
        );
    }
    let organization = otlp.organization.trim();
    if organization.is_empty() {
        bail!("config: observability.otlp.organization no puede estar vacía");
    }
    // La organización se interpola en la ruta `/api/{organization}/v1/{signal}`:
    // un espacio o una barra romperían la URL por señal.
    if organization.contains('/') || organization.chars().any(char::is_whitespace) {
        bail!("config: observability.otlp.organization no puede contener espacios ni '/'");
    }
    // `protocol: grpc` se parsea (el valor existe en el enum), pero el
    // transporte implementado es solo OTLP/HTTP: rechazarlo es mejor que
    // ignorarlo en silencio.
    if otlp.protocol == Protocol::Grpc {
        bail!("config: observability.otlp.protocol: 'grpc' aún no está soportado; usa 'http'");
    }
    if otlp.interval_ms == Some(0) {
        bail!("config: observability.otlp.interval_ms = 0; debe ser mayor que 0");
    }
    if otlp.timeout_ms == Some(0) {
        bail!("config: observability.otlp.timeout_ms = 0; debe ser mayor que 0");
    }
    if !otlp.signals.any() {
        bail!(
            "config: observability.otlp.signals requiere al menos una señal activa (metrics, logs o traces)"
        );
    }
    if let Some(var) = &otlp.token_env
        && std::env::var(var)
            .map(|value| value.is_empty())
            .unwrap_or(true)
    {
        bail!(
            "config: observability.otlp.token_env='{var}' pero la variable de entorno no existe o está vacía"
        );
    }
    Ok(())
}

/// Carga la configuración desde `GATEWAY_CONFIG` o `./gateway.yaml`.
///
/// # Errors
/// Igual que [`load`].
pub fn load_default() -> Result<GatewayConfig> {
    let path = config_path();
    load(&path)
}

/// Devuelve la ruta de configuración: `GATEWAY_CONFIG` si está definida, o la
/// ruta por defecto.
fn config_path() -> PathBuf {
    match std::env::var(GATEWAY_CONFIG_ENV) {
        Ok(value) if !value.trim().is_empty() => PathBuf::from(value),
        _ => PathBuf::from(DEFAULT_CONFIG_PATH),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::{
        BackendKind, DEFAULT_BLACKLIST_COOLDOWN_MS, DEFAULT_MIN_PRICE_DELTA,
        DEFAULT_QUOTA_POLL_INTERVAL_MS, DEFAULT_REFRESH_INTERVAL_MS, DEFAULT_RESELECT_INTERVAL_MS,
        Profile, load,
    };

    fn write_temp(contents: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::Builder::new()
            .suffix(".yaml")
            .tempfile()
            .expect("create temp file");
        file.write_all(contents.as_bytes())
            .expect("write temp file");
        file.flush().expect("flush temp file");
        file
    }

    fn dynamic_yaml(discovery: &str) -> String {
        format!(
            r#"
route:
  - name: mech-writer
    backend:
      - kind: openrouter_free
        base_url: "https://openrouter.ai/api/v1"
        api_key_env: "OPENROUTER_API_KEY"
        discovery: {discovery}
"#
        )
    }

    const SHARED_BASE_URL: &str = "https://openrouter.ai/api/v1";

    /// Bloque YAML de un backend dinámico; `ttl` a `None` omite `discovery`.
    fn dynamic_backend(base_url: &str, ttl: Option<u64>) -> String {
        let discovery = match ttl {
            Some(ms) => format!("        discovery: {{ refresh_interval_ms: {ms} }}\n"),
            None => String::new(),
        };
        format!(
            "      - kind: openrouter_free\n        base_url: \"{base_url}\"\n        api_key_env: \"OPENROUTER_API_KEY\"\n{discovery}"
        )
    }

    /// Ruta YAML con nombre y los bloques de backend ya renderizados.
    fn route_yaml(name: &str, backends: &[String]) -> String {
        format!("  - name: {name}\n    backend:\n{}", backends.concat())
    }

    /// Scenario: `discovery` vacío habilita el gate y el sondeo por defecto.
    #[test]
    fn empty_discovery_enables_daily_quota_with_default_poll_interval() {
        let file = write_temp(&dynamic_yaml("{}"));

        let config = load(file.path()).expect("empty discovery should load");

        let discovery = config.route[0].backend[0]
            .discovery
            .as_ref()
            .expect("discovery block should be present");
        assert!(
            discovery.respect_daily_quota,
            "respect_daily_quota debe ser true por defecto"
        );
        assert_eq!(
            discovery.quota_poll_interval_ms, DEFAULT_QUOTA_POLL_INTERVAL_MS,
            "el intervalo de sondeo debe tomar el default"
        );
    }

    /// Scenario: valores explícitos de gate y sondeo se respetan.
    #[test]
    fn explicit_respect_and_poll_interval_are_parsed() {
        let file = write_temp(&dynamic_yaml(
            "{ respect_daily_quota: false, quota_poll_interval_ms: 5000 }",
        ));

        let config = load(file.path()).expect("explicit quota fields should load");

        let discovery = config.route[0].backend[0]
            .discovery
            .as_ref()
            .expect("discovery block should be present");
        assert!(!discovery.respect_daily_quota);
        assert_eq!(discovery.quota_poll_interval_ms, 5_000);
    }

    /// Scenario: `quota_poll_interval_ms: 0` es error de configuración.
    #[test]
    fn explicit_zero_quota_poll_interval_is_rejected() {
        let file = write_temp(&dynamic_yaml("{ quota_poll_interval_ms: 0 }"));

        let err = load(file.path()).expect_err("zero poll interval should be rejected");
        let message = err.to_string();

        assert!(message.contains("mech-writer"), "message: {message}");
        assert!(
            message.contains("quota_poll_interval_ms"),
            "el mensaje debe nombrar el campo: {message}"
        );
    }

    /// Scenario: `discovery` con solo `min_context`
    #[test]
    fn discovery_with_only_min_context_uses_default_values() {
        let file = write_temp(&dynamic_yaml("{ min_context: 32768 }"));

        let config = load(file.path()).expect("discovery with only min_context should load");

        let discovery = config.route[0].backend[0]
            .discovery
            .as_ref()
            .expect("discovery block should be present");
        assert_eq!(discovery.refresh_interval_ms, DEFAULT_REFRESH_INTERVAL_MS);
        assert_eq!(
            discovery.blacklist_cooldown_ms,
            DEFAULT_BLACKLIST_COOLDOWN_MS
        );
        assert_eq!(discovery.min_context, Some(32_768));
    }

    /// Scenario: `discovery` vacío
    #[test]
    fn empty_discovery_uses_default_values() {
        let file = write_temp(&dynamic_yaml("{}"));

        let config = load(file.path()).expect("empty discovery should load");

        let discovery = config.route[0].backend[0]
            .discovery
            .as_ref()
            .expect("discovery block should be present");
        assert_eq!(discovery.refresh_interval_ms, DEFAULT_REFRESH_INTERVAL_MS);
        assert_eq!(
            discovery.blacklist_cooldown_ms,
            DEFAULT_BLACKLIST_COOLDOWN_MS
        );
        assert_eq!(discovery.min_context, None);
    }

    /// Scenario: TTL explícito mantiene el default del cooldown
    #[test]
    fn explicit_refresh_interval_keeps_default_blacklist_cooldown() {
        let file = write_temp(&dynamic_yaml("{ refresh_interval_ms: 1500 }"));

        let config = load(file.path()).expect("explicit refresh interval should load");

        let discovery = config.route[0].backend[0]
            .discovery
            .as_ref()
            .expect("discovery block should be present");
        assert_eq!(discovery.refresh_interval_ms, 1_500);
        assert_eq!(
            discovery.blacklist_cooldown_ms,
            DEFAULT_BLACKLIST_COOLDOWN_MS
        );
    }

    /// Scenario: cooldown explícito mantiene el default del refresh interval
    #[test]
    fn explicit_blacklist_cooldown_keeps_default_refresh_interval() {
        let file = write_temp(&dynamic_yaml("{ blacklist_cooldown_ms: 5000 }"));

        let config = load(file.path()).expect("explicit blacklist cooldown should load");

        let discovery = config.route[0].backend[0]
            .discovery
            .as_ref()
            .expect("discovery block should be present");
        assert_eq!(
            discovery.blacklist_cooldown_ms, 5_000,
            "el cooldown explícito debe respetarse"
        );
        assert_eq!(
            discovery.refresh_interval_ms, DEFAULT_REFRESH_INTERVAL_MS,
            "el refresh interval debe tomar el default"
        );
    }

    /// Scenario: cooldown cero explícito
    #[test]
    fn explicit_zero_blacklist_cooldown_is_rejected() {
        let file = write_temp(&dynamic_yaml("{ blacklist_cooldown_ms: 0 }"));

        let err = load(file.path()).expect_err("zero blacklist cooldown should be rejected");
        let message = err.to_string();

        assert!(message.contains("mech-writer"), "message: {message}");
        assert!(
            message.contains("https://openrouter.ai/api/v1#dynamic"),
            "message: {message}"
        );
        assert!(
            message.contains("blacklist_cooldown_ms"),
            "message: {message}"
        );
    }

    /// Scenario: TTL cero explícito
    #[test]
    fn explicit_zero_refresh_interval_is_rejected() {
        let file = write_temp(&dynamic_yaml("{ refresh_interval_ms: 0 }"));

        let err = load(file.path()).expect_err("zero refresh interval should be rejected");
        let message = err.to_string();

        assert!(message.contains("mech-writer"), "message: {message}");
        assert!(
            message.contains("https://openrouter.ai/api/v1#dynamic"),
            "message: {message}"
        );
        assert!(
            message.contains("refresh_interval_ms"),
            "message: {message}"
        );
    }

    /// Scenario: TTL incoherente por `base_url` en una misma ruta.
    #[test]
    fn ttl_conflict_in_same_route_is_rejected() {
        let yaml = format!(
            "route:\n{}",
            route_yaml(
                "mech-writer",
                &[
                    dynamic_backend(SHARED_BASE_URL, Some(1_500)),
                    dynamic_backend(SHARED_BASE_URL, Some(3_000)),
                ],
            )
        );
        let file = write_temp(&yaml);

        let err = load(file.path()).expect_err("distinct TTLs for one base_url should be rejected");
        let message = err.to_string();

        assert!(message.contains(SHARED_BASE_URL), "message: {message}");
        assert!(message.contains("mech-writer"), "message: {message}");
        assert!(
            message.contains("discovery.refresh_interval_ms"),
            "el mensaje debe nombrar el campo: {message}"
        );
        assert!(
            message.contains("se esperaba 1500 ms"),
            "el mensaje debe indicar el TTL esperado: {message}"
        );
        assert!(
            message.contains("se encontró 3000 ms"),
            "el mensaje debe indicar el TTL encontrado: {message}"
        );
    }

    /// Misma ruta, dos dinámicos con la misma `base_url` y el mismo TTL.
    #[test]
    fn same_route_equal_ttls_are_accepted() {
        let yaml = format!(
            "route:\n{}",
            route_yaml(
                "mech-writer",
                &[
                    dynamic_backend(SHARED_BASE_URL, Some(1_500)),
                    dynamic_backend(SHARED_BASE_URL, Some(1_500)),
                ],
            )
        );
        let file = write_temp(&yaml);

        load(file.path()).expect("equal TTLs within one route should load");
    }

    /// El nombre de una ruta con dos backends en conflicto aparece una sola vez.
    #[test]
    fn same_route_conflict_names_route_once() {
        let yaml = format!(
            "route:\n{}",
            route_yaml(
                "mech-writer",
                &[
                    dynamic_backend(SHARED_BASE_URL, Some(1_500)),
                    dynamic_backend(SHARED_BASE_URL, Some(3_000)),
                ],
            )
        );
        let file = write_temp(&yaml);

        let err = load(file.path()).expect_err("distinct TTLs for one base_url should be rejected");
        let message = err.to_string();

        assert_eq!(
            message.matches("mech-writer").count(),
            1,
            "la ruta debe nombrarse una sola vez: {message}"
        );
    }

    /// Un backend estático con la misma `base_url` no entra en la validación de TTL.
    #[test]
    fn static_backend_sharing_base_url_is_not_ttl_validated() {
        let static_backend = format!(
            "      - kind: static\n        base_url: \"{SHARED_BASE_URL}\"\n        model: \"some/model\"\n"
        );
        let yaml = format!(
            "route:\n{}",
            route_yaml(
                "mech-writer",
                &[
                    dynamic_backend(SHARED_BASE_URL, Some(1_500)),
                    static_backend
                ],
            )
        );
        let file = write_temp(&yaml);

        let config =
            load(file.path()).expect("a static backend must not take part in the TTL check");
        let dynamic = &config.route[0].backend[0];
        assert_eq!(
            dynamic
                .discovery
                .as_ref()
                .map(|discovery| discovery.refresh_interval_ms),
            Some(1_500)
        );
    }

    /// Scenario: TTL incoherente por `base_url` entre rutas distintas.
    #[test]
    fn ttl_conflict_across_routes_names_all_routes() {
        let yaml = format!(
            "route:\n{}{}",
            route_yaml(
                "mech-writer",
                &[dynamic_backend(SHARED_BASE_URL, Some(1_500))],
            ),
            route_yaml(
                "mech-code",
                &[dynamic_backend(SHARED_BASE_URL, Some(3_000))],
            ),
        );
        let file = write_temp(&yaml);

        let err = load(file.path()).expect_err("distinct TTLs across routes should be rejected");
        let message = err.to_string();

        assert!(message.contains(SHARED_BASE_URL), "message: {message}");
        assert!(message.contains("mech-writer"), "message: {message}");
        assert!(message.contains("mech-code"), "message: {message}");
        assert!(message.contains("1500"), "message: {message}");
        assert!(message.contains("3000"), "message: {message}");
    }

    /// Scenario: TTL coherente por `base_url`.
    #[test]
    fn equal_ttls_for_same_base_url_are_accepted() {
        let yaml = format!(
            "route:\n{}{}",
            route_yaml(
                "mech-writer",
                &[dynamic_backend(SHARED_BASE_URL, Some(1_500))],
            ),
            route_yaml(
                "mech-code",
                &[dynamic_backend(SHARED_BASE_URL, Some(1_500))],
            ),
        );
        let file = write_temp(&yaml);

        load(file.path()).expect("equal TTLs for one base_url should load");
    }

    /// Scenario: backend sin `discovery` y otro con el TTL por defecto.
    #[test]
    fn missing_discovery_matches_explicit_default_ttl() {
        let yaml = format!(
            "route:\n{}{}",
            route_yaml("mech-writer", &[dynamic_backend(SHARED_BASE_URL, None)]),
            route_yaml(
                "mech-code",
                &[dynamic_backend(
                    SHARED_BASE_URL,
                    Some(DEFAULT_REFRESH_INTERVAL_MS),
                )],
            ),
        );
        let file = write_temp(&yaml);

        load(file.path()).expect("explicit default TTL should match the implicit default");
    }

    /// Scenario: backend sin `discovery` frente a un TTL explícito distinto.
    #[test]
    fn missing_discovery_conflicts_with_non_default_ttl() {
        let yaml = format!(
            "route:\n{}{}",
            route_yaml("mech-writer", &[dynamic_backend(SHARED_BASE_URL, None)]),
            route_yaml(
                "mech-code",
                &[dynamic_backend(SHARED_BASE_URL, Some(1_500))],
            ),
        );
        let file = write_temp(&yaml);

        let err = load(file.path())
            .expect_err("implicit default TTL should conflict with an explicit non-default TTL");
        let message = err.to_string();

        assert!(message.contains(SHARED_BASE_URL), "message: {message}");
        assert!(message.contains("mech-writer"), "message: {message}");
        assert!(message.contains("mech-code"), "message: {message}");
        assert!(
            message.contains(&DEFAULT_REFRESH_INTERVAL_MS.to_string()),
            "message: {message}"
        );
        assert!(message.contains("1500"), "message: {message}");
    }

    /// Ambos `base_url` conservan TTLs independientes.
    #[test]
    fn distinct_base_urls_keep_independent_ttls() {
        let yaml = format!(
            "route:\n{}{}",
            route_yaml(
                "mech-writer",
                &[dynamic_backend("https://openrouter.ai/api/v1", Some(1_500),)],
            ),
            route_yaml(
                "mech-code",
                &[dynamic_backend(
                    "https://mirror.example.com/api/v1",
                    Some(3_000),
                )],
            ),
        );
        let file = write_temp(&yaml);

        load(file.path()).expect("distinct base_urls should keep independent TTLs");
    }

    /// Backend de pago con el bloque `discovery` inyectado tal cual (indentado).
    fn paid_yaml(discovery_block: &str) -> String {
        format!(
            r#"
route:
  - name: mech-code
    backend:
      - kind: openrouter_paid
        profile: code
        base_url: "{SHARED_BASE_URL}"
        api_key_env: "OPENROUTER_API_KEY"
        discovery:
{discovery_block}
"#
        )
    }

    /// Scenario 5.1: `openrouter_paid` se parsea como backend dinámico con su
    /// perfil y sin exigir `model`.
    #[test]
    fn openrouter_paid_is_parsed_with_profile_and_without_model() {
        let file = write_temp(&paid_yaml("          min_context: 32768"));

        let config = load(file.path()).expect("el backend de pago debe cargar");

        let backend = &config.route[0].backend[0];
        assert_eq!(backend.kind, BackendKind::OpenrouterPaid);
        assert!(backend.is_dynamic(), "el backend de pago es dinámico");
        assert!(
            !backend.is_free_tier(),
            "el backend de pago no pertenece al tier free"
        );
        assert_eq!(backend.profile(), Profile::Code);
        assert_eq!(
            backend.model, None,
            "el modelo explícito se ignora en dinámicos"
        );
    }

    /// Scenario 5.1: `openrouter_paid` sin `api_key_env` es error de configuración.
    #[test]
    fn openrouter_paid_requires_api_key_env() {
        let yaml = format!(
            r#"
route:
  - name: mech-code
    backend:
      - kind: openrouter_paid
        profile: code
        base_url: "{SHARED_BASE_URL}"
"#
        );
        let file = write_temp(&yaml);

        let err = load(file.path()).expect_err("sin api_key_env debe fallar");
        let message = err.to_string();

        assert!(message.contains("mech-code"), "message: {message}");
        assert!(message.contains("openrouter_paid"), "message: {message}");
        assert!(message.contains("api_key_env"), "message: {message}");
    }

    /// Scenario 5.1: un backend de pago sin `base_url` no deserializa.
    #[test]
    fn openrouter_paid_without_base_url_is_rejected() {
        let yaml = r#"
route:
  - name: mech-code
    backend:
      - kind: openrouter_paid
        profile: code
        api_key_env: "OPENROUTER_API_KEY"
"#;
        let file = write_temp(yaml);

        let err = load(file.path()).expect_err("sin base_url debe fallar");

        assert!(
            err.to_string().contains("base_url"),
            "el error debe nombrar el campo ausente: {err}"
        );
    }

    /// Scenario 5.2: defaults de `allow`, `reselect_interval_ms` y `min_price_delta`.
    #[test]
    fn paid_discovery_defaults_are_applied() {
        let file = write_temp(&paid_yaml("          min_context: 32768"));

        let config = load(file.path()).expect("discovery de pago con defaults debe cargar");
        let discovery = config.route[0].backend[0]
            .discovery
            .as_ref()
            .expect("bloque discovery presente");

        assert!(
            discovery.allow.is_empty(),
            "sin `allow` no hay lista curada por defecto en el código"
        );
        assert_eq!(
            discovery.reselect_interval_ms, DEFAULT_RESELECT_INTERVAL_MS,
            "la cadencia de reselección debe tomar el default"
        );
        assert_eq!(
            discovery.min_price_delta, DEFAULT_MIN_PRICE_DELTA,
            "la mejora mínima debe tomar el default"
        );
    }

    /// Scenario 5.2: los campos de pago explícitos se respetan tal cual.
    #[test]
    fn paid_discovery_explicit_fields_are_parsed() {
        let file = write_temp(&paid_yaml(
            "          allow: [\"z-ai/glm-5.*-flash\", \"qwen/qwen3.7-flash\"]\n          reselect_interval_ms: 600000\n          min_price_delta: 0.5",
        ));

        let config = load(file.path()).expect("discovery de pago explícito debe cargar");
        let discovery = config.route[0].backend[0]
            .discovery
            .as_ref()
            .expect("bloque discovery presente");

        assert_eq!(
            discovery.allow,
            vec![
                "z-ai/glm-5.*-flash".to_owned(),
                "qwen/qwen3.7-flash".to_owned()
            ],
            "la lista curada conserva el orden declarado"
        );
        assert_eq!(discovery.reselect_interval_ms, 600_000);
        assert_eq!(discovery.min_price_delta, 0.5);
    }

    /// Scenario: Patrón vacío es error (backend de pago).
    #[test]
    fn empty_allow_pattern_is_rejected_for_paid_backend() {
        let file = write_temp(&paid_yaml("          allow: [\"\"]"));

        let err = load(file.path()).expect_err("un patrón vacío debe rechazarse");
        let message = err.to_string();

        assert!(message.contains("mech-code"), "message: {message}");
        assert!(message.contains("allow"), "message: {message}");
        assert!(message.contains("vacío"), "message: {message}");
    }

    /// Un patrón vacío también es error en un backend free: la lista curada es
    /// una invariante de configuración, no del tier.
    #[test]
    fn empty_allow_pattern_is_rejected_for_free_backend() {
        let file = write_temp(&dynamic_yaml("{ allow: [\"\"] }"));

        let err = load(file.path()).expect_err("un patrón vacío debe rechazarse");
        let message = err.to_string();

        assert!(message.contains("mech-writer"), "message: {message}");
        assert!(message.contains("allow"), "message: {message}");
    }

    /// Scenario 5.2: `min_price_delta` negativo es error de configuración.
    #[test]
    fn negative_min_price_delta_is_rejected() {
        let file = write_temp(&paid_yaml("          min_price_delta: -0.1"));

        let err = load(file.path()).expect_err("una mejora negativa no tiene sentido");
        let message = err.to_string();

        assert!(message.contains("mech-code"), "message: {message}");
        assert!(message.contains("min_price_delta"), "message: {message}");
    }
}
