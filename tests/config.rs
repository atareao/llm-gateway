use std::io::Write;

use llm_gateway::config::load;

const VALID_YAML: &str = r#"
route:
  - name: mech-writer
    backend:
      - base_url: "http://localhost:11434/v1"
        model: "qwen2.5:7b-instruct"
      - base_url: "https://openrouter.ai/api/v1"
        model: "qwen/qwen3-8b"
        api_key_env: "OPENROUTER_API_KEY"
        timeout_ms: 30000
"#;

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

/// Scenario: Config válida
#[test]
fn valid_config_loads_routes_and_backends_in_order() {
    let file = write_temp(VALID_YAML);

    let config = load(file.path()).expect("valid config should load");

    assert_eq!(config.route.len(), 1);
    let route = &config.route[0];
    assert_eq!(route.name, "mech-writer");
    assert_eq!(route.backend.len(), 2);
    assert_eq!(route.backend[0].base_url, "http://localhost:11434/v1");
    assert_eq!(route.backend[0].model, "qwen2.5:7b-instruct");
    assert_eq!(route.backend[0].api_key_env, None);
    assert_eq!(route.backend[1].base_url, "https://openrouter.ai/api/v1");
    assert_eq!(route.backend[1].model, "qwen/qwen3-8b");
    assert_eq!(
        route.backend[1].api_key_env.as_deref(),
        Some("OPENROUTER_API_KEY")
    );
    assert_eq!(route.backend[1].timeout_ms, Some(30_000));
}

/// Scenario: Ruta sin backends
#[test]
fn route_without_backends_is_rejected_naming_the_route() {
    let yaml = r#"
route:
  - name: mech-writer
    backend: []
"#;
    let file = write_temp(yaml);

    let err = load(file.path()).expect_err("route without backends must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("mech-writer"),
        "error must mention the route name, got: {msg}"
    );
}

/// Scenario: Fichero ausente
#[test]
fn missing_file_is_rejected() {
    let path = std::path::Path::new("/nonexistent/definitely-not-here-12345.yaml");
    let err = load(path).expect_err("missing file must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("definitely-not-here-12345"),
        "error should mention the path, got: {msg}"
    );
}

/// Scenario 1.1a: backend sin bloque `warmup` → `warmup` es `None`.
#[test]
fn backend_without_warmup_has_none() {
    let file = write_temp(VALID_YAML);

    let config = load(file.path()).expect("valid config should load");

    assert!(
        config.route[0].backend[0].warmup.is_none(),
        "un backend sin bloque `warmup` debe quedar en None"
    );
    assert!(
        config.route[0].backend[1].warmup.is_none(),
        "tampoco el segundo backend debe tener warmup"
    );
}

const WARMUP_YAML: &str = r#"
route:
  - name: mech-writer
    backend:
      - base_url: "http://localhost:11434/v1"
        model: "qwen2.5:7b-instruct"
        warmup:
          interval_ms: 240000
"#;

/// Scenario 1.1b: `warmup.interval_ms` presente se parsea y conserva.
#[test]
fn warmup_interval_ms_is_parsed() {
    let file = write_temp(WARMUP_YAML);

    let config = load(file.path()).expect("valid config should load");

    let warmup = config.route[0].backend[0]
        .warmup
        .as_ref()
        .expect("el backend declarado debe conservar su warmup");
    assert_eq!(warmup.interval_ms, 240_000);
}

/// Scenario 1.1c: `warmup.interval_ms = 0` → error que nombra ruta y backend.
#[test]
fn warmup_interval_zero_is_rejected() {
    let yaml = r#"
route:
  - name: mech-writer
    backend:
      - base_url: "http://warmup-zero.invalid/v1"
        model: "warmup-zero-model"
        warmup:
          interval_ms: 0
"#;
    let file = write_temp(yaml);

    let err = load(file.path()).expect_err("interval_ms 0 debe ser rechazado");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("mech-writer"),
        "el error debe nombrar la ruta afectada, got: {msg}"
    );
    assert!(
        msg.contains("warmup-zero"),
        "el error debe nombrar el backend afectado, got: {msg}"
    );
}

/// Scenario 1.1d: `warmup.timeout_ms` ausente → `None` (se usará el default).
#[test]
fn warmup_timeout_ms_absent_is_none() {
    let file = write_temp(WARMUP_YAML);

    let config = load(file.path()).expect("valid config should load");

    let warmup = config.route[0].backend[0]
        .warmup
        .as_ref()
        .expect("el backend declarado debe conservar su warmup");
    assert_eq!(warmup.interval_ms, 240_000);
    assert_eq!(
        warmup.timeout_ms, None,
        "sin `timeout_ms`, el warmup debe quedar en None"
    );
}

const WARMUP_TIMEOUT_YAML: &str = r#"
route:
  - name: mech-writer
    backend:
      - base_url: "http://localhost:11434/v1"
        model: "qwen2.5:7b-instruct"
        warmup:
          interval_ms: 240000
          timeout_ms: 300000
"#;

/// Scenario 1.1e: `warmup.timeout_ms` presente se parsea y conserva.
#[test]
fn warmup_timeout_ms_is_parsed() {
    let file = write_temp(WARMUP_TIMEOUT_YAML);

    let config = load(file.path()).expect("valid config should load");

    let warmup = config.route[0].backend[0]
        .warmup
        .as_ref()
        .expect("el backend declarado debe conservar su warmup");
    assert_eq!(warmup.interval_ms, 240_000);
    assert_eq!(warmup.timeout_ms, Some(300_000));
}

/// Scenario 1.1f: `warmup.timeout_ms = 0` → error que nombra ruta y backend.
#[test]
fn warmup_timeout_zero_is_rejected() {
    let yaml = r#"
route:
  - name: mech-writer
    backend:
      - base_url: "http://warmup-timeout-zero.invalid/v1"
        model: "warmup-timeout-zero-model"
        warmup:
          interval_ms: 240000
          timeout_ms: 0
"#;
    let file = write_temp(yaml);

    let err = load(file.path()).expect_err("timeout_ms 0 debe ser rechazado");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("mech-writer"),
        "el error debe nombrar la ruta afectada, got: {msg}"
    );
    assert!(
        msg.contains("warmup-timeout-zero"),
        "el error debe nombrar el backend afectado, got: {msg}"
    );
}

/// Scenario: YAML inválido
#[test]
fn invalid_yaml_reports_line_information() {
    // Indentación rota: un backend sin `backend:` correcto.
    let yaml = "route:\n  - name: mech-writer\n   backend: [\n";
    let file = write_temp(yaml);

    let err = load(file.path()).expect_err("invalid yaml must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("line"),
        "error must include line information, got: {msg}"
    );
}
