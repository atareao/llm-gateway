//! Carga y validación de la configuración de rutas del gateway (YAML).

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

/// Configuración completa del gateway.
#[derive(Debug, Deserialize)]
pub struct GatewayConfig {
    /// Rutas lógicas que el cliente puede pedir como `model`.
    pub route: Vec<Route>,
    /// Bloque opcional de observabilidad (exportación OTLP).
    pub observability: Option<Observability>,
}

/// Configuración de observabilidad. Ausente en el YAML → `None` y el gateway
/// no inicializa ningún exportador.
#[derive(Debug, Deserialize)]
pub struct Observability {
    /// Exportación OTLP opcional.
    pub otlp: Option<Otlp>,
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

/// Configuración de descubrimiento de un backend `openrouter_free`.
#[derive(Debug, Clone, Deserialize)]
pub struct Discovery {
    /// TTL de la caché del catálogo y periodo de refresco en segundo plano.
    pub refresh_interval_ms: u64,
    /// Tiempo que un modelo permanece en la blacklist tras un fallo.
    pub blacklist_cooldown_ms: u64,
    /// Contexto mínimo (en tokens) exigido a los modelos elegibles.
    pub min_context: Option<u32>,
    /// Reclasificaciones manuales por perfil (opcional).
    #[serde(default)]
    pub overrides: Option<ClassifyOverrides>,
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
}

impl Backend {
    /// `true` si el backend es dinámico (`openrouter_free`).
    pub fn is_dynamic(&self) -> bool {
        self.kind == BackendKind::OpenrouterFree
    }

    /// Perfil efectivo del backend dinámico (por defecto [`Profile::Code`]).
    pub fn profile(&self) -> Profile {
        self.profile.unwrap_or_default()
    }

    /// Etiqueta legible del backend para mensajes de error, en el formato
    /// `<base_url>#<model|dynamic>`.
    pub fn label(&self) -> String {
        let model = self.model.as_deref().unwrap_or("dynamic");
        format!("{}#{}", self.base_url, model)
    }
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
/// por sí solo: cada ruta debe declarar al menos un backend, y el bloque
/// opcional `observability.otlp` debe ser coherente.
fn validate(config: &GatewayConfig) -> Result<()> {
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
                BackendKind::OpenrouterFree => {
                    if backend.api_key_env.as_deref().is_none_or(str::is_empty) {
                        bail!(
                            "config: la ruta '{}' backend '{}' es de tipo openrouter_free y requiere 'api_key_env'",
                            route.name,
                            backend.label()
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
