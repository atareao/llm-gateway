//! Carga y validación de la configuración de rutas del gateway (YAML).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// Variable de entorno que sobrescribe la ruta del fichero de configuración.
pub const GATEWAY_CONFIG_ENV: &str = "GATEWAY_CONFIG";

/// Ruta por defecto del fichero de configuración.
pub const DEFAULT_CONFIG_PATH: &str = "gateway.yaml";

/// Configuración completa del gateway.
#[derive(Debug, Deserialize)]
pub struct GatewayConfig {
    /// Rutas lógicas que el cliente puede pedir como `model`.
    pub route: Vec<Route>,
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
#[derive(Debug, Deserialize)]
pub struct Backend {
    /// URL base de la API compatible con OpenAI (sin `/chat/completions`).
    pub base_url: String,
    /// Modelo real que entiende este backend.
    pub model: String,
    /// Nombre de la variable de entorno que contiene la API key (opcional).
    pub api_key_env: Option<String>,
    /// Timeout de la petición en milisegundos (opcional).
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
/// por sí solo: cada ruta debe declarar al menos un backend.
fn validate(config: &GatewayConfig) -> Result<()> {
    for route in &config.route {
        if route.backend.is_empty() {
            bail!("config: la ruta '{}' no tiene backends", route.name);
        }
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
