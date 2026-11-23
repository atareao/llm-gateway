//! Clasificación de modelos free por perfil (`architect` / `code` / `writer`).
//!
//! A partir del catálogo ya filtrado a modelos elegibles, [`classify`] deriva
//! una lista **ordenada** de identificadores por perfil aplicando una heurística
//! sobre `id`/`name`/`description`/`context_length`, con soporte de `min_context`
//! y de reclasificaciones manuales (`exclude`/`include`).

use std::collections::HashSet;

use serde_json::Value;

use crate::config::{ClassifyOverrides, Profile};
use crate::discovery::{ModelEntry, Pricing, supports_tool_calling};

/// Contexto de clasificación: perfil, filtros y procedencia del catálogo.
///
/// `base_url` solo interviene en la trazabilidad del aviso de `include`
/// ignorado; no afecta a la elegibilidad ni al orden.
#[derive(Debug, Clone, Copy)]
pub struct ClassifyContext<'a> {
    /// Perfil de uso al que se clasifica el catálogo.
    pub profile: Profile,
    /// Contexto mínimo (en tokens) exigido a los modelos elegibles.
    pub min_context: Option<u32>,
    /// Reclasificaciones manuales (`exclude`/`include`).
    pub overrides: Option<&'a ClassifyOverrides>,
    /// `base_url` de la que procede el catálogo, usada en los avisos.
    pub base_url: &'a str,
}

impl<'a> ClassifyContext<'a> {
    /// Construye el contexto de clasificación.
    pub const fn new(
        profile: Profile,
        min_context: Option<u32>,
        overrides: Option<&'a ClassifyOverrides>,
        base_url: &'a str,
    ) -> Self {
        Self {
            profile,
            min_context,
            overrides,
            base_url,
        }
    }
}

/// Clasifica `models` según el `context` y devuelve los identificadores ordenados.
///
/// Orden: por *tier* de preferencia del perfil (ascendente), y dentro de cada
/// tier por `context_length` descendente y, a igualdad, por `id` ascendente.
/// Los modelos con `context_length` inferior a `min_context` se descartan. Los
/// `include` de `overrides` se anteponen en su orden; los `exclude` se eliminan.
///
/// Cada `include` no elegible (ausente del catálogo, filtrado por `min_context`
/// o excluido por `exclude`) se descarta y emite un aviso con el modelo, el
/// perfil y la `base_url`, sin alterar la lista resultante.
pub fn classify(models: &[ModelEntry], context: ClassifyContext<'_>) -> Vec<String> {
    let excluded = excluded_ids(context.overrides);

    let mut scored: Vec<(u8, u32, &str)> = models
        .iter()
        .filter(|model| is_eligible(model, context.min_context, &excluded))
        .map(|model| {
            let haystack = haystack(model);
            (
                tier(context.profile, &haystack),
                model.context_length.unwrap_or(0),
                model.id.as_str(),
            )
        })
        .collect();

    scored.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)).then(a.2.cmp(b.2)));

    let mut ordered: Vec<String> = Vec::new();
    if let Some(overrides) = context.overrides {
        for id in ignored_includes(models, context.min_context, Some(overrides)) {
            tracing::warn!(
                model = %id,
                profile = %context.profile.label(),
                base_url = %context.base_url,
                "include ignorado: modelo no elegible"
            );
        }
        for include in &overrides.include {
            if scored.iter().any(|(_, _, id)| *id == include.as_str())
                && !ordered.iter().any(|id| id == include)
            {
                ordered.push(include.clone());
            }
        }
    }
    for (_, _, id) in scored {
        if !ordered.iter().any(|existing| existing == id) {
            ordered.push(id.to_owned());
        }
    }

    ordered
}

/// Contexto de clasificación de un backend de pago (`openrouter_paid`).
///
/// A diferencia de [`ClassifyContext`], no hay perfil ni `overrides`: la
/// curación la decide la lista `allow` y el orden lo decide el precio.
#[derive(Debug, Clone, Copy)]
pub struct PaidContext<'a> {
    /// Contexto mínimo (en tokens) exigido a los modelos elegibles.
    pub min_context: Option<u32>,
    /// Patrones glob (`*` = cero o más caracteres) sobre el `model_id`.
    pub allow: &'a [String],
}

impl<'a> PaidContext<'a> {
    /// Construye el contexto de clasificación de pago.
    pub const fn new(min_context: Option<u32>, allow: &'a [String]) -> Self {
        Self { min_context, allow }
    }
}

/// `true` si `model_id` coincide con alguno de los patrones de `allow`.
///
/// El comodín `*` coincide con cero o más caracteres; el resto de caracteres se
/// comparan literalmente. Una lista vacía no coincide con nada.
pub fn matches_allow(model_id: &str, allow: &[String]) -> bool {
    allow.iter().any(|pattern| glob_matches(pattern, model_id))
}

/// Coincidencia glob con `*` (cero o más caracteres) sobre el identificador.
///
/// Dos punteros con retroceso al último `*` visto: sin asignaciones por carácter
/// y sin expresiones regulares.
fn glob_matches(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0, 0);
    let mut last_star: Option<usize> = None;
    let mut resume = 0;

    while t < text.len() {
        if p < pattern.len() && pattern[p] == '*' {
            last_star = Some(p);
            resume = t;
            p += 1;
        } else if p < pattern.len() && pattern[p] == text[t] {
            p += 1;
            t += 1;
        } else if let Some(star) = last_star {
            p = star + 1;
            resume += 1;
            t = resume;
        } else {
            return false;
        }
    }

    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

/// Precio de referencia de un modelo: `blend = 0.75 · prompt + 0.25 · completion`.
///
/// Un precio ausente o no numérico cuenta como `0`, igual que en el catálogo: un
/// modelo sin precios obtiene `blend = 0` y queda fuera de la elegibilidad por la
/// regla de precio estrictamente positivo.
pub fn blend_price(pricing: &Pricing) -> f64 {
    /// Peso del precio de prompt en el `blend`.
    const PROMPT_WEIGHT: f64 = 0.75;
    /// Peso del precio de completion en el `blend`.
    const COMPLETION_WEIGHT: f64 = 0.25;

    PROMPT_WEIGHT * price_value(pricing.prompt.as_ref())
        + COMPLETION_WEIGHT * price_value(pricing.completion.as_ref())
}

/// Parsea el precio de un campo del catálogo (`0` si falta o no es numérico).
fn price_value(value: Option<&Value>) -> f64 {
    match value {
        Some(Value::Number(number)) => number.as_f64().unwrap_or(0.0),
        Some(Value::String(text)) => text.trim().parse::<f64>().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// Clasifica `models` como candidatos de pago según el `context`.
///
/// Un modelo es elegible si coincide con algún patrón de `allow`, soporta
/// tool-calling, tiene precio estrictamente positivo (lo que excluye el tier
/// free), supera `min_context` y no es un modelo `:batch`. El orden es por
/// [`blend_price`] ascendente, con desempate por `context_length` descendente y
/// `id` ascendente.
///
/// El aviso por patrones de `allow` sin coincidencias no vive aquí: lo emite
/// [`crate::discovery::Registry`] una sola vez por `(base_url, patrón)`, fuera
/// del camino por petición.
pub fn classify_paid(models: &[ModelEntry], context: PaidContext<'_>) -> Vec<String> {
    let mut scored: Vec<(f64, u32, &str)> = models
        .iter()
        .filter(|model| is_paid_eligible(model, context.min_context, context.allow))
        .map(|model| {
            (
                model.pricing.as_ref().map_or(0.0, blend_price),
                model.context_length.unwrap_or(0),
                model.id.as_str(),
            )
        })
        .collect();

    scored.sort_by(|a, b| a.0.total_cmp(&b.0).then(b.1.cmp(&a.1)).then(a.2.cmp(b.2)));

    scored.into_iter().map(|(_, _, id)| id.to_owned()).collect()
}

/// `true` si `model` es elegible en un backend de pago.
fn is_paid_eligible(model: &ModelEntry, min_context: Option<u32>, allow: &[String]) -> bool {
    matches_allow(&model.id, allow)
        && supports_tool_calling(model)
        && has_paid_pricing(model)
        && min_context.is_none_or(|min| model.context_length.unwrap_or(0) >= min)
        && !model.id.ends_with(":batch")
}

/// `true` si el modelo declara algún precio estrictamente positivo.
///
/// Es la regla que excluye el tier free: `prompt > 0` o `completion > 0`.
fn has_paid_pricing(model: &ModelEntry) -> bool {
    model.pricing.as_ref().is_some_and(|pricing| {
        price_value(pricing.prompt.as_ref()) > 0.0 || price_value(pricing.completion.as_ref()) > 0.0
    })
}

/// Identificadores excluidos por `overrides`.
fn excluded_ids(overrides: Option<&ClassifyOverrides>) -> HashSet<&str> {
    overrides
        .map(|overrides| overrides.exclude.iter().map(String::as_str).collect())
        .unwrap_or_default()
}

/// `true` si `model` es elegible: supera `min_context` y no está en `excluded`.
///
/// Predicado único compartido por la lista resultante y por el cálculo de
/// `include` ignorados, para que ambos apliquen la misma noción de elegibilidad.
fn is_eligible(model: &ModelEntry, min_context: Option<u32>, excluded: &HashSet<&str>) -> bool {
    min_context.is_none_or(|min| model.context_length.unwrap_or(0) >= min)
        && !excluded.contains(model.id.as_str())
}

/// Identificadores de `overrides.include` que no son elegibles (ausentes del
/// catálogo, filtrados por `min_context` o en `exclude`), sin duplicados y en
/// orden de primera aparición.
fn ignored_includes(
    models: &[ModelEntry],
    min_context: Option<u32>,
    overrides: Option<&ClassifyOverrides>,
) -> Vec<String> {
    let Some(overrides) = overrides else {
        return Vec::new();
    };
    let excluded = excluded_ids(Some(overrides));
    let mut ignored: Vec<String> = Vec::new();
    for include in &overrides.include {
        let eligible = models.iter().any(|model| {
            model.id.as_str() == include.as_str() && is_eligible(model, min_context, &excluded)
        });
        if !eligible && !ignored.contains(include) {
            ignored.push(include.clone());
        }
    }
    ignored
}

/// Texto sobre el que se aplica la heurística (`id`, `name` y `description`).
fn haystack(model: &ModelEntry) -> String {
    let mut text = model.id.to_lowercase();
    if let Some(name) = &model.name {
        text.push(' ');
        text.push_str(&name.to_lowercase());
    }
    if let Some(description) = &model.description {
        text.push(' ');
        text.push_str(&description.to_lowercase());
    }
    text
}

/// Tier de preferencia del modelo dentro del perfil (menor = más preferido).
fn tier(profile: Profile, haystack: &str) -> u8 {
    match profile {
        Profile::Architect => {
            if is_reasoning(haystack) {
                0
            } else if is_large_general(haystack) {
                1
            } else {
                2
            }
        }
        Profile::Code => u8::from(!is_coder(haystack)),
        Profile::Writer => u8::from(is_coder(haystack)),
    }
}

/// `true` si el texto apunta a una familia de razonamiento.
fn is_reasoning(haystack: &str) -> bool {
    const NEEDLES: [&str; 5] = ["deepseek-r1", "deepseek/r1", "reasoner", "thinking", "qwq"];
    NEEDLES.iter().any(|needle| haystack.contains(needle)) || has_o_series(haystack)
}

/// `true` si el texto contiene un token de la serie `o` de OpenAI (`o1`, `o3`…):
/// una `o` seguida de un dígito, precedida de un límite no alfanumérico.
fn has_o_series(haystack: &str) -> bool {
    let bytes = haystack.as_bytes();
    bytes.iter().enumerate().any(|(index, byte)| {
        *byte == b'o'
            && bytes.get(index + 1).is_some_and(u8::is_ascii_digit)
            && (index == 0 || !bytes[index - 1].is_ascii_alphanumeric())
    })
}

/// `true` si el texto apunta a un modelo general grande.
fn is_large_general(haystack: &str) -> bool {
    const NEEDLES: [&str; 7] = [
        "70b",
        "72b",
        "235b",
        "405b",
        "deepseek-v3",
        "deepseek/v3",
        "llama-3.3",
    ];
    NEEDLES.iter().any(|needle| haystack.contains(needle))
}

/// `true` si el texto apunta a una familia orientada a código.
fn is_coder(haystack: &str) -> bool {
    const NEEDLES: [&str; 4] = ["coder", "codestral", "devstral", "codeqwen"];
    NEEDLES.iter().any(|needle| haystack.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::{
        ClassifyContext, PaidContext, blend_price, classify, classify_paid, ignored_includes,
        matches_allow,
    };
    use crate::config::{ClassifyOverrides, Profile};
    use crate::discovery::{ModelEntry, Pricing};
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing::{Event, Level, Subscriber};
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
    use tracing_subscriber::registry;

    /// Modelo elegible con nombre y contexto.
    fn model(id: &str, name: &str, context_length: u32) -> ModelEntry {
        ModelEntry {
            id: id.to_owned(),
            name: Some(name.to_owned()),
            description: None,
            context_length: Some(context_length),
            pricing: Some(Pricing {
                prompt: Some(json!("0")),
                completion: Some(json!("0")),
            }),
            supported_parameters: Some(vec!["tools".to_owned(), "tool_choice".to_owned()]),
        }
    }

    /// `base_url` de procedencia usada por los tests.
    const BASE_URL: &str = "https://openrouter.ai/api/v1";

    /// Clasifica con el contexto por defecto de los tests (sin filtros).
    fn classify_with(models: &[ModelEntry], profile: Profile) -> Vec<String> {
        classify(models, ClassifyContext::new(profile, None, None, BASE_URL))
    }

    /// Clasifica aplicando solo `min_context`.
    fn classify_with_min_context(
        models: &[ModelEntry],
        profile: Profile,
        min_context: u32,
    ) -> Vec<String> {
        classify(
            models,
            ClassifyContext::new(profile, Some(min_context), None, BASE_URL),
        )
    }

    /// Clasifica aplicando solo los overrides indicados.
    fn classify_with_overrides(
        models: &[ModelEntry],
        profile: Profile,
        overrides: &ClassifyOverrides,
    ) -> Vec<String> {
        classify(
            models,
            ClassifyContext::new(profile, None, Some(overrides), BASE_URL),
        )
    }

    fn reasoning() -> ModelEntry {
        model("deepseek/deepseek-r1:free", "DeepSeek R1", 64_000)
    }
    fn large_general() -> ModelEntry {
        model(
            "meta-llama/llama-3.3-70b-instruct",
            "Llama 3.3 70B",
            131_072,
        )
    }
    fn coder() -> ModelEntry {
        model("qwen/qwen3-coder:free", "Qwen3 Coder", 262_144)
    }
    fn general() -> ModelEntry {
        model("google/gemma-2-9b-it:free", "Gemma 2 9B", 8_192)
    }

    /// Scenario: `architect` prioriza razonamiento.
    #[test]
    fn architect_prioritizes_reasoning() {
        let models = vec![general(), large_general(), reasoning()];
        let order = classify_with(&models, Profile::Architect);
        assert_eq!(order[0], "deepseek/deepseek-r1:free");
    }

    /// Scenario: `architect` cae a general grande.
    #[test]
    fn architect_falls_back_to_large_general() {
        let models = vec![general(), large_general()];
        let order = classify_with(&models, Profile::Architect);
        assert_eq!(order[0], "meta-llama/llama-3.3-70b-instruct");
    }

    /// Scenario: `code` prioriza familias coder.
    #[test]
    fn code_prioritizes_coder_families() {
        let models = vec![large_general(), coder()];
        let order = classify_with(&models, Profile::Code);
        assert_eq!(order[0], "qwen/qwen3-coder:free");
    }

    /// Scenario: `writer` prioriza instruct general frente a coder.
    #[test]
    fn writer_prioritizes_general_over_coder() {
        let models = vec![coder(), general()];
        let order = classify_with(&models, Profile::Writer);
        assert_eq!(order[0], "google/gemma-2-9b-it:free");
        assert_eq!(order[1], "qwen/qwen3-coder:free");
    }

    /// Scenario: Orden determinista por `context_length` desc y `id` asc.
    #[test]
    fn order_is_by_context_desc_then_id() {
        let models = vec![
            model("b/small:free", "small", 1_000),
            model("a/big:free", "big", 2_000),
            model("c/big:free", "big2", 2_000),
        ];
        let order = classify_with(&models, Profile::Code);
        assert_eq!(
            order,
            vec!["a/big:free", "c/big:free", "b/small:free"],
            "mayor contexto primero y, a igualdad, id ascendente"
        );
    }

    /// Scenario: `min_context` descarta modelos pequeños.
    #[test]
    fn min_context_discards_small_models() {
        let models = vec![general(), large_general()];
        for profile in [Profile::Architect, Profile::Code, Profile::Writer] {
            let order = classify_with_min_context(&models, profile, 32_768);
            assert!(
                !order.contains(&"google/gemma-2-9b-it:free".to_owned()),
                "el modelo por debajo de min_context no debe aparecer en {profile:?}: {order:?}"
            );
            assert!(
                order.contains(&"meta-llama/llama-3.3-70b-instruct".to_owned()),
                "el modelo por encima de min_context debe seguir en {profile:?}: {order:?}"
            );
        }
    }

    /// Scenario: Override manual excluye un modelo del perfil.
    #[test]
    fn override_excludes_model_from_profile() {
        let models = vec![coder(), general()];
        let overrides = ClassifyOverrides {
            exclude: vec!["qwen/qwen3-coder:free".to_owned()],
            include: vec![],
        };
        let order = classify_with_overrides(&models, Profile::Code, &overrides);
        assert!(
            !order.contains(&"qwen/qwen3-coder:free".to_owned()),
            "el modelo excluido no debe aparecer: {order:?}"
        );
    }

    /// Override `include` antepone el modelo indicado.
    #[test]
    fn override_include_pins_model_to_front() {
        let models = vec![coder(), general()];
        let include = "google/gemma-2-9b-it:free";
        let overrides = ClassifyOverrides {
            exclude: vec![],
            include: vec![include.to_owned()],
        };
        let order = classify_with_overrides(&models, Profile::Code, &overrides);
        assert_eq!(order[0], include);
        assert_eq!(
            order.iter().filter(|id| id.as_str() == include).count(),
            1,
            "el include debe aparecer exactamente una vez y no duplicarse: {order:?}"
        );
    }

    /// Overrides con `include`/`exclude` a partir de literales.
    fn overrides(include: &[&str], exclude: &[&str]) -> ClassifyOverrides {
        ClassifyOverrides {
            include: include.iter().map(|id| (*id).to_owned()).collect(),
            exclude: exclude.iter().map(|id| (*id).to_owned()).collect(),
        }
    }

    /// Un `include` presente y elegible no se ignora.
    #[test]
    fn ignored_includes_is_empty_when_include_is_eligible() {
        let models = vec![general()];
        let overrides = overrides(&["google/gemma-2-9b-it:free"], &[]);

        assert_eq!(
            ignored_includes(&models, None, Some(&overrides)),
            Vec::<String>::new()
        );
    }

    /// Un `include` ausente del catálogo se ignora una vez.
    #[test]
    fn ignored_includes_reports_model_absent_from_catalog() {
        let models = vec![general()];
        let overrides = overrides(&["missing/model:free"], &[]);

        assert_eq!(
            ignored_includes(&models, Some(32_768), Some(&overrides)),
            vec!["missing/model:free".to_owned()]
        );
    }

    /// Un `include` filtrado por `min_context` se ignora.
    #[test]
    fn ignored_includes_reports_model_filtered_by_min_context() {
        let models = vec![general()];
        let overrides = overrides(&["google/gemma-2-9b-it:free"], &[]);

        assert_eq!(
            ignored_includes(&models, Some(32_768), Some(&overrides)),
            vec!["google/gemma-2-9b-it:free".to_owned()]
        );
        assert_eq!(
            ignored_includes(&models, Some(8_192), Some(&overrides)),
            Vec::<String>::new(),
            "justo en el umbral el modelo sigue siendo elegible"
        );
    }

    /// Un `include` que también está en `exclude` se ignora (`exclude` gana,
    /// coherente con la lista resultante).
    #[test]
    fn ignored_includes_reports_model_excluded_by_override() {
        let models = vec![general()];
        let overrides = overrides(
            &["google/gemma-2-9b-it:free"],
            &["google/gemma-2-9b-it:free"],
        );

        assert_eq!(
            ignored_includes(&models, None, Some(&overrides)),
            vec!["google/gemma-2-9b-it:free".to_owned()]
        );
    }

    /// Los `include` repetidos se deduplican en el orden de primera aparición.
    #[test]
    fn ignored_includes_deduplicates_repeated_ids() {
        let models = vec![general()];
        let overrides = overrides(&["missing/model:free", "missing/model:free"], &[]);

        assert_eq!(
            ignored_includes(&models, None, Some(&overrides)),
            vec!["missing/model:free".to_owned()]
        );
    }

    /// Sin `overrides` no hay ningún `include` que ignorar.
    #[test]
    fn ignored_includes_is_empty_without_overrides() {
        let models = vec![general()];

        assert!(ignored_includes(&models, None, None).is_empty());
    }

    /// Capa mínima de `tracing` que acumula los eventos con su nivel y sus campos.
    #[derive(Clone)]
    struct CaptureLayer {
        events: Arc<Mutex<Vec<(Level, String)>>>,
    }

    impl<S: Subscriber> Layer<S> for CaptureLayer {
        fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
            let mut visitor = TextVisitor::default();
            event.record(&mut visitor);
            self.events
                .lock()
                .expect("lock captured events")
                .push((*event.metadata().level(), visitor.fields.join(" ")));
        }
    }

    /// Visitante que serializa los campos del evento como `campo=valor`.
    #[derive(Default)]
    struct TextVisitor {
        fields: Vec<String>,
    }

    impl Visit for TextVisitor {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.fields.push(format!("{}={value:?}", field.name()));
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            self.fields.push(format!("{}={value}", field.name()));
        }
    }

    /// Subscriber *scoped* (no global) y los eventos que capture.
    struct Capture {
        events: Arc<Mutex<Vec<(Level, String)>>>,
        _guard: tracing::subscriber::DefaultGuard,
    }

    impl Capture {
        /// Campos de los eventos capturados en nivel `WARN`.
        fn warnings(&self) -> Vec<String> {
            self.fields_at(Level::WARN)
        }

        /// Campos de los eventos capturados en nivel `ERROR`.
        fn errors(&self) -> Vec<String> {
            self.fields_at(Level::ERROR)
        }

        /// Campos de los eventos capturados en el nivel indicado.
        fn fields_at(&self, level: Level) -> Vec<String> {
            self.events
                .lock()
                .expect("lock captured events")
                .iter()
                .filter(|(event_level, _)| *event_level == level)
                .map(|(_, fields)| fields.clone())
                .collect()
        }
    }

    /// Instala una captura de avisos solo para el hilo actual.
    fn capture_warnings() -> Capture {
        let events = Arc::new(Mutex::new(Vec::new()));
        let subscriber = registry().with(CaptureLayer {
            events: Arc::clone(&events),
        });
        Capture {
            events,
            _guard: tracing::subscriber::set_default(subscriber),
        }
    }

    /// Scenario: `include` no elegible avisa y se descarta.
    #[test]
    fn non_eligible_include_emits_warning_with_model_profile_and_base_url() {
        let models = vec![general()];
        let overrides = overrides(&["missing/model:free"], &[]);
        let capture = capture_warnings();

        let order = classify(
            &models,
            ClassifyContext::new(Profile::Writer, None, Some(&overrides), BASE_URL),
        );

        assert_eq!(order, vec!["google/gemma-2-9b-it:free".to_owned()]);
        let warnings = capture.warnings();
        let warning = warnings
            .iter()
            .find(|event| event.contains("include ignorado"))
            .unwrap_or_else(|| panic!("no se emitió ningún aviso: {warnings:?}"));
        assert!(
            warning.contains("model=missing/model:free"),
            "el aviso debe incluir el modelo: {warning}"
        );
        assert!(
            warning.contains("profile=writer"),
            "el aviso debe incluir el perfil: {warning}"
        );
        assert!(
            warning.contains(BASE_URL),
            "el aviso debe incluir la base_url: {warning}"
        );
    }

    /// Scenario: `include` antepone un modelo elegible (sin aviso).
    #[test]
    fn eligible_include_does_not_emit_warning() {
        let models = vec![general()];
        let overrides = overrides(&["google/gemma-2-9b-it:free"], &[]);
        let capture = capture_warnings();

        let order = classify(
            &models,
            ClassifyContext::new(Profile::Code, None, Some(&overrides), BASE_URL),
        );

        assert_eq!(order[0], "google/gemma-2-9b-it:free");
        assert!(
            capture.warnings().is_empty(),
            "un include elegible no debe avisar: {:?}",
            capture.warnings()
        );
    }

    /// Scenario: `include` filtrado por `min_context` avisa.
    #[test]
    fn min_context_filtered_include_emits_warning() {
        let models = vec![general()];
        let overrides = overrides(&["google/gemma-2-9b-it:free"], &[]);
        let capture = capture_warnings();

        let order = classify(
            &models,
            ClassifyContext::new(Profile::Code, Some(32_768), Some(&overrides), BASE_URL),
        );

        assert!(order.is_empty(), "el catálogo queda vacío: {order:?}");
        let warnings = capture.warnings();
        assert!(
            warnings
                .iter()
                .any(|event| event.contains("google/gemma-2-9b-it:free")),
            "el aviso debe nombrar el modelo filtrado: {warnings:?}"
        );
    }

    /// Un identificador repetido en `include` produce un único aviso.
    #[test]
    fn duplicate_include_emits_single_warning() {
        let models = vec![general()];
        let overrides = overrides(&["missing/model:free", "missing/model:free"], &[]);
        let capture = capture_warnings();

        classify(
            &models,
            ClassifyContext::new(Profile::Code, None, Some(&overrides), BASE_URL),
        );

        let warnings = capture.warnings();
        assert_eq!(
            warnings.len(),
            1,
            "un aviso por identificador ignorado: {warnings:?}"
        );
    }

    /// Lista curada de los tests (subconjunto de la lista por defecto).
    fn curated() -> Vec<String> {
        vec![
            "z-ai/glm-5.*-flash".to_owned(),
            "qwen/qwen3.7-flash".to_owned(),
        ]
    }

    /// Modelo de pago con precios, contexto y tool-calling explícitos.
    fn paid_model(id: &str, prompt: &str, completion: &str, context_length: u32) -> ModelEntry {
        ModelEntry {
            id: id.to_owned(),
            name: None,
            description: None,
            context_length: Some(context_length),
            pricing: Some(Pricing {
                prompt: Some(json!(prompt)),
                completion: Some(json!(completion)),
            }),
            supported_parameters: Some(vec!["tools".to_owned(), "tool_choice".to_owned()]),
        }
    }

    /// Clasifica modelos de pago con la lista curada y sin `min_context`.
    fn classify_curated(models: &[ModelEntry]) -> Vec<String> {
        let allow = curated();
        classify_paid(models, PaidContext::new(None, &allow))
    }

    /// Scenario: El comodín absorbe versiones menores (y admite cero caracteres).
    #[test]
    fn matches_allow_handles_wildcards() {
        let allow = curated();

        assert!(
            matches_allow("z-ai/glm-5.3-flash", &allow),
            "el comodín debe cubrir la versión menor"
        );
        assert!(
            matches_allow("qwen/qwen3.7-flash", &allow),
            "un patrón sin comodines es un match literal"
        );
        assert!(
            !matches_allow("z-ai/glm-5-flash", &allow),
            "sin el punto que ancla el comodín no hay coincidencia"
        );
        assert!(
            !matches_allow("qwen/qwen3.7-flash-pro", &allow),
            "el patrón debe cubrir el identificador completo"
        );

        let trailing = vec!["deepseek/deepseek-v4.1-flash*".to_owned()];
        assert!(
            matches_allow("deepseek/deepseek-v4.1-flash", &trailing),
            "el comodín final admite cero caracteres"
        );
        assert!(matches_allow(
            "deepseek/deepseek-v4.1-flash-2026",
            &trailing
        ));

        assert!(matches_allow("cualquiera/cosa", &["*".to_owned()]));
        assert!(
            !matches_allow("cualquiera/cosa", &[]),
            "sin patrones no hay coincidencia posible"
        );
    }

    /// El blend pondera 0.75 el prompt y 0.25 la completion.
    #[test]
    fn blend_price_weights_prompt_and_completion() {
        let pricing = Pricing {
            prompt: Some(json!("0.5")),
            completion: Some(json!("0.25")),
        };
        assert_eq!(blend_price(&pricing), 0.4375);

        let free = Pricing {
            prompt: Some(json!("0")),
            completion: Some(json!("0")),
        };
        assert_eq!(blend_price(&free), 0.0, "un modelo free tiene blend 0");

        assert_eq!(
            blend_price(&Pricing::default()),
            0.0,
            "un modelo sin precios tiene blend 0"
        );
    }

    /// Scenario: Orden por precio ascendente.
    #[test]
    fn classify_paid_orders_by_blend_ascending() {
        let models = vec![
            paid_model("qwen/qwen3.7-flash", "0.25", "0.125", 1_000_000),
            paid_model("z-ai/glm-5.3-flash", "0.0625", "0.03125", 131_072),
        ];

        let order = classify_curated(&models);

        assert_eq!(
            order,
            vec!["z-ai/glm-5.3-flash", "qwen/qwen3.7-flash"],
            "el modelo más barato va primero aunque tenga menos contexto"
        );
    }

    /// Scenario: Excluye el tier free.
    #[test]
    fn classify_paid_excludes_free_models() {
        let free_suffix = paid_model("qwen/qwen3.7-flash:free", "0", "0", 131_072);
        let models = vec![
            paid_model("z-ai/glm-5.3-flash", "0", "0", 131_072),
            free_suffix,
            paid_model("z-ai/glm-5.1-flash", "0.0625", "0", 131_072),
        ];

        let order = classify_curated(&models);

        assert_eq!(
            order,
            vec!["z-ai/glm-5.1-flash"],
            "solo entra el modelo con precio estrictamente positivo: {order:?}"
        );
    }

    /// Scenario: Exige tool-calling y contexto mínimo.
    #[test]
    fn classify_paid_requires_tool_calling_and_min_context() {
        let mut only_tools = paid_model("z-ai/glm-5.3-flash", "0.0625", "0.03125", 131_072);
        only_tools.supported_parameters = Some(vec!["tools".to_owned()]);
        let mut without_tools = paid_model("z-ai/glm-5.3-flash", "0.0625", "0.03125", 131_072);
        without_tools.supported_parameters = Some(vec!["temperature".to_owned()]);
        let too_small = paid_model("z-ai/glm-5.4-flash", "0.0625", "0.03125", 1_000);

        let allow = curated();
        let order = classify_paid(
            &[only_tools, without_tools, too_small],
            PaidContext::new(Some(32_768), &allow),
        );
        assert!(
            order.is_empty(),
            "sin tools+tool_choice o bajo min_context quedan fuera: {order:?}"
        );

        let boundary = paid_model("z-ai/glm-5.5-flash", "0.0625", "0.03125", 32_768);
        let order = classify_paid(&[boundary], PaidContext::new(Some(32_768), &allow));
        assert_eq!(
            order,
            vec!["z-ai/glm-5.5-flash"],
            "justo en el umbral el modelo sigue siendo elegible"
        );
    }

    /// Un modelo `:batch` queda excluido aunque algún patrón lo cubra.
    #[test]
    fn classify_paid_excludes_batch_models() {
        let allow = vec!["z-ai/*".to_owned()];
        let models = vec![
            paid_model("z-ai/glm-5.3-flash", "0.0625", "0.03125", 131_072),
            paid_model("z-ai/glm-5.3-flash:batch", "0.0625", "0.03125", 131_072),
        ];

        let order = classify_paid(&models, PaidContext::new(None, &allow));

        assert_eq!(order, vec!["z-ai/glm-5.3-flash"]);
    }

    /// A igualdad de blend: mayor contexto primero y, a igualdad, `id` ascendente.
    #[test]
    fn classify_paid_ties_break_by_context_desc_then_id_asc() {
        let allow = vec!["lab/*".to_owned()];
        let models = vec![
            paid_model("lab/small", "0.0625", "0.03125", 1_000),
            paid_model("lab/big-b", "0.0625", "0.03125", 200_000),
            paid_model("lab/big-a", "0.0625", "0.03125", 200_000),
        ];

        let order = classify_paid(&models, PaidContext::new(None, &allow));

        assert_eq!(order, vec!["lab/big-a", "lab/big-b", "lab/small"]);
    }

    /// Sin lista curada el backend de pago no aporta candidatos.
    #[test]
    fn classify_paid_without_allow_yields_no_candidates() {
        let models = vec![paid_model(
            "z-ai/glm-5.3-flash",
            "0.0625",
            "0.03125",
            131_072,
        )];

        assert!(
            classify_paid(&models, PaidContext::new(None, &[])).is_empty(),
            "sin `allow` no hay ningún modelo elegible"
        );
    }

    /// `classify_paid` no emite avisos: el aviso por patrón huérfano vive en el
    /// `Registry`, una vez por `(base_url, patrón)` y fuera del hot path.
    #[test]
    fn classify_paid_does_not_warn_on_unmatched_patterns() {
        let models = vec![paid_model(
            "z-ai/glm-5.3-flash",
            "0.0625",
            "0.03125",
            131_072,
        )];
        let allow = vec![
            "lab-inexistente/modelo-x".to_owned(),
            "z-ai/glm-5.*-flash".to_owned(),
        ];
        let capture = capture_warnings();

        let order = classify_paid(&models, PaidContext::new(None, &allow));

        assert_eq!(
            order,
            vec!["z-ai/glm-5.3-flash"],
            "el patrón con coincidencias sigue aportando candidatos"
        );
        assert!(
            capture.warnings().is_empty(),
            "la clasificación no avisa; eso es responsabilidad del Registry: {:?}",
            capture.warnings()
        );
        assert!(
            capture.errors().is_empty(),
            "un patrón sin coincidencias no es un error: {:?}",
            capture.errors()
        );
    }
}
