//! Clasificación de modelos free por perfil (`architect` / `code` / `writer`).
//!
//! A partir del catálogo ya filtrado a modelos elegibles, [`classify`] deriva
//! una lista **ordenada** de identificadores por perfil aplicando una heurística
//! sobre `id`/`name`/`description`/`context_length`, con soporte de `min_context`
//! y de reclasificaciones manuales (`exclude`/`include`).

use std::collections::HashSet;

use crate::config::{ClassifyOverrides, Profile};
use crate::discovery::ModelEntry;

/// Clasifica `models` para `profile` y devuelve los identificadores ordenados.
///
/// Orden: por *tier* de preferencia del perfil (ascendente), y dentro de cada
/// tier por `context_length` descendente y, a igualdad, por `id` ascendente.
/// Los modelos con `context_length` inferior a `min_context` se descartan. Los
/// `include` de `overrides` se anteponen en su orden; los `exclude` se eliminan.
pub fn classify(
    models: &[ModelEntry],
    profile: Profile,
    min_context: Option<u32>,
    overrides: Option<&ClassifyOverrides>,
) -> Vec<String> {
    let excluded: HashSet<&str> = overrides
        .map(|overrides| overrides.exclude.iter().map(String::as_str).collect())
        .unwrap_or_default();

    let mut scored: Vec<(u8, u32, &str)> = models
        .iter()
        .filter(|model| min_context.is_none_or(|min| model.context_length.unwrap_or(0) >= min))
        .filter(|model| !excluded.contains(model.id.as_str()))
        .map(|model| {
            let haystack = haystack(model);
            (
                tier(profile, &haystack),
                model.context_length.unwrap_or(0),
                model.id.as_str(),
            )
        })
        .collect();

    scored.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)).then(a.2.cmp(b.2)));

    let mut ordered: Vec<String> = Vec::new();
    if let Some(overrides) = overrides {
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
    use super::classify;
    use crate::config::{ClassifyOverrides, Profile};
    use crate::discovery::{ModelEntry, Pricing};
    use serde_json::json;

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
        let order = classify(&models, Profile::Architect, None, None);
        assert_eq!(order[0], "deepseek/deepseek-r1:free");
    }

    /// Scenario: `architect` cae a general grande.
    #[test]
    fn architect_falls_back_to_large_general() {
        let models = vec![general(), large_general()];
        let order = classify(&models, Profile::Architect, None, None);
        assert_eq!(order[0], "meta-llama/llama-3.3-70b-instruct");
    }

    /// Scenario: `code` prioriza familias coder.
    #[test]
    fn code_prioritizes_coder_families() {
        let models = vec![large_general(), coder()];
        let order = classify(&models, Profile::Code, None, None);
        assert_eq!(order[0], "qwen/qwen3-coder:free");
    }

    /// Scenario: `writer` prioriza instruct general frente a coder.
    #[test]
    fn writer_prioritizes_general_over_coder() {
        let models = vec![coder(), general()];
        let order = classify(&models, Profile::Writer, None, None);
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
        let order = classify(&models, Profile::Code, None, None);
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
        let order = classify(&models, Profile::Code, Some(32_768), None);
        assert!(
            !order.contains(&"google/gemma-2-9b-it:free".to_owned()),
            "el modelo por debajo de min_context no debe aparecer: {order:?}"
        );
        assert!(order.contains(&"meta-llama/llama-3.3-70b-instruct".to_owned()));
    }

    /// Scenario: Override manual excluye un modelo del perfil.
    #[test]
    fn override_excludes_model_from_profile() {
        let models = vec![coder(), general()];
        let overrides = ClassifyOverrides {
            exclude: vec!["qwen/qwen3-coder:free".to_owned()],
            include: vec![],
        };
        let order = classify(&models, Profile::Code, None, Some(&overrides));
        assert!(
            !order.contains(&"qwen/qwen3-coder:free".to_owned()),
            "el modelo excluido no debe aparecer: {order:?}"
        );
    }

    /// Override `include` antepone el modelo indicado.
    #[test]
    fn override_include_pins_model_to_front() {
        let models = vec![coder(), general()];
        let overrides = ClassifyOverrides {
            exclude: vec![],
            include: vec!["google/gemma-2-9b-it:free".to_owned()],
        };
        let order = classify(&models, Profile::Code, None, Some(&overrides));
        assert_eq!(order[0], "google/gemma-2-9b-it:free");
    }
}
