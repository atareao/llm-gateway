use std::io::Write;

use llm_gateway_rs::config::{BackendKind, Profile, Protocol, load};

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
    assert_eq!(
        route.backend[0].model.as_deref(),
        Some("qwen2.5:7b-instruct")
    );
    assert_eq!(route.backend[0].api_key_env, None);
    assert_eq!(route.backend[1].base_url, "https://openrouter.ai/api/v1");
    assert_eq!(route.backend[1].model.as_deref(), Some("qwen/qwen3-8b"));
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

// ---------------------------------------------------------------------------
// Bloque opcional `observability.otlp` (change add-otlp-observability)
// ---------------------------------------------------------------------------

fn with_observability(block: &str) -> String {
    format!(
        r#"
route:
  - name: mech-writer
    backend:
      - base_url: "http://localhost:11434/v1"
        model: "qwen2.5:7b-instruct"
{block}
"#
    )
}

/// Scenario: Ausencia del bloque no cambia nada → `observability` es `None`.
#[test]
fn observability_absent_is_none() {
    let file = write_temp(VALID_YAML);

    let config = load(file.path()).expect("valid config should load");

    assert!(
        config.observability.is_none(),
        "sin bloque `observability`, debe quedar en None"
    );
}

/// Scenario: bloque válido se parsea con todos sus campos.
#[test]
fn observability_otlp_is_parsed() {
    // Nombre de variable único por test: varios tests corren en paralelo en el
    // mismo proceso y una variable compartida se pisaría entre ellos.
    let token_var = format!("GATEWAY_TEST_PARSED_OTLP_TOKEN_{}", std::process::id());
    let yaml = with_observability(&format!(
        r#"observability:
  otlp:
    endpoint: "http://localhost:5080"
    organization: "default"
    protocol: http
    token_env: "{token_var}"
    service_name: "llm-gateway"
    interval_ms: 5000
    timeout_ms: 1000
    signals:
      metrics: true
      logs: false
      traces: true
"#
    ));
    // La variable referenciada debe existir para que la validación pase.
    // SAFETY: nombre único por test; el valor no contiene datos sensibles.
    unsafe { std::env::set_var(&token_var, "abc123") };
    let file = write_temp(&yaml);

    let config = load(file.path()).expect("bloque válido debe cargar");
    unsafe { std::env::remove_var(&token_var) };

    let otlp = config
        .observability
        .as_ref()
        .and_then(|observability| observability.otlp.as_ref())
        .expect("el bloque otlp debe conservarse");

    assert_eq!(otlp.endpoint, "http://localhost:5080");
    assert_eq!(otlp.organization, "default");
    assert_eq!(otlp.protocol, Protocol::Http);
    assert_eq!(otlp.token_env.as_deref(), Some(token_var.as_str()));
    assert_eq!(otlp.service_name.as_deref(), Some("llm-gateway"));
    assert_eq!(otlp.interval_ms, Some(5_000));
    assert_eq!(otlp.timeout_ms, Some(1_000));
    assert!(otlp.signals.metrics, "signals.metrics debe ser true");
    assert!(!otlp.signals.logs, "signals.logs debe ser false");
    assert!(otlp.signals.traces, "signals.traces debe ser true");
}

/// Scenario: `protocol: grpc` se rechaza mientras no esté implementado.
#[test]
fn observability_grpc_protocol_is_rejected() {
    let yaml = with_observability(
        r#"observability:
  otlp:
    endpoint: "http://localhost:5080"
    organization: "default"
    protocol: grpc
    signals:
      metrics: true
"#,
    );
    let file = write_temp(&yaml);

    let err = load(file.path()).expect_err("grpc aún no soportado debe fallar");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("grpc") && msg.contains("soportado"),
        "el error debe explicar que grpc no está soportado, got: {msg}"
    );
}

/// Scenario: `protocol` ausente → por defecto `Http`.
#[test]
fn observability_protocol_defaults_to_http() {
    let yaml = with_observability(
        r#"observability:
  otlp:
    endpoint: "http://localhost:5080"
    organization: "default"
    signals:
      metrics: true
"#,
    );
    let file = write_temp(&yaml);

    let config = load(file.path()).expect("bloque válido debe cargar");
    let otlp = config.observability.unwrap().otlp.unwrap();

    assert_eq!(otlp.protocol, Protocol::Http);
    assert_eq!(otlp.token_env, None);
    assert_eq!(otlp.service_name, None);
    assert_eq!(otlp.interval_ms, None);
    assert_eq!(otlp.timeout_ms, None);
}

/// Scenario: Endpoint vacío se rechaza.
#[test]
fn observability_endpoint_empty_is_rejected() {
    let yaml = with_observability(
        r#"observability:
  otlp:
    endpoint: ""
    organization: "default"
    signals:
      metrics: true
"#,
    );
    let file = write_temp(&yaml);

    let err = load(file.path()).expect_err("endpoint vacío debe fallar");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("endpoint"),
        "el error debe mencionar endpoint, got: {msg}"
    );
}

/// Scenario: Endpoint en blanco se rechaza.
#[test]
fn observability_endpoint_blank_is_rejected() {
    let yaml = with_observability(
        r#"observability:
  otlp:
    endpoint: "   "
    organization: "default"
    signals:
      metrics: true
"#,
    );
    let file = write_temp(&yaml);

    let err = load(file.path()).expect_err("endpoint en blanco debe fallar");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("endpoint"),
        "el error debe mencionar endpoint, got: {msg}"
    );
}

/// Scenario: Organization vacía se rechaza.
#[test]
fn observability_organization_empty_is_rejected() {
    let yaml = with_observability(
        r#"observability:
  otlp:
    endpoint: "http://localhost:5080"
    organization: ""
    signals:
      metrics: true
"#,
    );
    let file = write_temp(&yaml);

    let err = load(file.path()).expect_err("organization vacía debe fallar");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("organization"),
        "el error debe mencionar organization, got: {msg}"
    );
}

/// Scenario: endpoint sin esquema `http`/`https` se rechaza.
#[test]
fn observability_endpoint_without_scheme_is_rejected() {
    let yaml = with_observability(
        r#"observability:
  otlp:
    endpoint: "localhost:5080"
    organization: "default"
    signals:
      metrics: true
"#,
    );
    let file = write_temp(&yaml);

    let err = load(file.path()).expect_err("endpoint sin esquema debe fallar");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("endpoint") && msg.contains("http"),
        "el error debe pedir un endpoint http/https, got: {msg}"
    );
}

/// Scenario: `organization` con espacios se rechaza.
#[test]
fn observability_organization_with_space_is_rejected() {
    let yaml = with_observability(
        r#"observability:
  otlp:
    endpoint: "http://localhost:5080"
    organization: "mi org"
    signals:
      metrics: true
"#,
    );
    let file = write_temp(&yaml);

    let err = load(file.path()).expect_err("organization con espacios debe fallar");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("organization"),
        "el error debe mencionar organization, got: {msg}"
    );
}

/// Scenario: `organization` con `/` se rechaza (rompería la URL por señal).
#[test]
fn observability_organization_with_slash_is_rejected() {
    let yaml = with_observability(
        r#"observability:
  otlp:
    endpoint: "http://localhost:5080"
    organization: "mi/org"
    signals:
      metrics: true
"#,
    );
    let file = write_temp(&yaml);

    let err = load(file.path()).expect_err("organization con '/' debe fallar");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("organization"),
        "el error debe mencionar organization, got: {msg}"
    );
}

/// Scenario: Intervalo cero se rechaza.
#[test]
fn observability_interval_zero_is_rejected() {
    let yaml = with_observability(
        r#"observability:
  otlp:
    endpoint: "http://localhost:5080"
    organization: "default"
    interval_ms: 0
    signals:
      metrics: true
"#,
    );
    let file = write_temp(&yaml);

    let err = load(file.path()).expect_err("interval_ms = 0 debe fallar");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("interval_ms"),
        "el error debe mencionar interval_ms, got: {msg}"
    );
}

/// Scenario: Timeout cero se rechaza.
#[test]
fn observability_timeout_zero_is_rejected() {
    let yaml = with_observability(
        r#"observability:
  otlp:
    endpoint: "http://localhost:5080"
    organization: "default"
    timeout_ms: 0
    signals:
      metrics: true
"#,
    );
    let file = write_temp(&yaml);

    let err = load(file.path()).expect_err("timeout_ms = 0 debe fallar");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("timeout_ms"),
        "el error debe mencionar timeout_ms, got: {msg}"
    );
}

/// Scenario: todas las señales a `false` se rechaza.
#[test]
fn observability_all_signals_false_is_rejected() {
    let yaml = with_observability(
        r#"observability:
  otlp:
    endpoint: "http://localhost:5080"
    organization: "default"
    signals:
      metrics: false
      logs: false
      traces: false
"#,
    );
    let file = write_temp(&yaml);

    let err = load(file.path()).expect_err("sin señales activas debe fallar");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("signal") || msg.contains("señal"),
        "el error debe mencionar las señales, got: {msg}"
    );
}

/// Scenario: Token de ingesta inexistente se rechaza.
#[test]
fn observability_token_env_missing_is_rejected() {
    let var = format!("GATEWAY_TEST_MISSING_OTLP_TOKEN_{}", std::process::id());
    let yaml = with_observability(&format!(
        r#"observability:
  otlp:
    endpoint: "http://localhost:5080"
    organization: "default"
    token_env: "{var}"
    signals:
      metrics: true
"#
    ));
    let file = write_temp(&yaml);

    let err = load(file.path()).expect_err("token_env sin variable debe fallar");
    let msg = format!("{err:#}");
    assert!(
        msg.contains(&var),
        "el error debe nombrar la variable, got: {msg}"
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

// ---------------------------------------------------------------------------
// Backends dinámicos `openrouter_free` (change add-openrouter-free-routing)
// ---------------------------------------------------------------------------

const DYNAMIC_YAML: &str = r#"
route:
  - name: code
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "https://openrouter.ai/api/v1"
        api_key_env: "OPENROUTER_API_KEY"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: 60000
          min_context: 32768
      - base_url: "https://openrouter.ai/api/v1"
        model: "deepseek/deepseek-v4.1-flash"
        api_key_env: "OPENROUTER_API_KEY"
"#;

/// Scenario: Backend dinámico válido → se acepta con perfil `code`.
#[test]
fn dynamic_backend_is_accepted_with_profile() {
    let file = write_temp(DYNAMIC_YAML);

    let config = load(file.path()).expect("config dinámica válida debe cargar");

    let dynamic = &config.route[0].backend[0];
    assert_eq!(dynamic.kind, BackendKind::OpenrouterFree);
    assert_eq!(dynamic.profile, Some(Profile::Code));
    assert_eq!(dynamic.base_url, "https://openrouter.ai/api/v1");
    assert_eq!(dynamic.api_key_env.as_deref(), Some("OPENROUTER_API_KEY"));
    assert_eq!(dynamic.model, None, "un backend dinámico ignora `model`");

    let discovery = dynamic
        .discovery
        .as_ref()
        .expect("el bloque discovery debe conservarse");
    assert_eq!(discovery.refresh_interval_ms, 300_000);
    assert_eq!(discovery.blacklist_cooldown_ms, 60_000);
    assert_eq!(discovery.min_context, Some(32_768));
}

/// Scenario: Backend estático sin `kind` → se interpreta como `static`.
#[test]
fn backend_without_kind_defaults_to_static() {
    let file = write_temp(VALID_YAML);

    let config = load(file.path()).expect("config estática válida debe cargar");

    for backend in &config.route[0].backend {
        assert_eq!(
            backend.kind,
            BackendKind::Static,
            "sin `kind`, el backend debe ser estático"
        );
    }
    assert_eq!(
        config.route[0].backend[0].model.as_deref(),
        Some("qwen2.5:7b-instruct")
    );
}

/// Scenario: Backend dinámico sin `profile` → perfil por defecto `code`.
#[test]
fn dynamic_backend_defaults_profile_to_code() {
    let yaml = r#"
route:
  - name: code
    backend:
      - kind: openrouter_free
        base_url: "https://openrouter.ai/api/v1"
        api_key_env: "OPENROUTER_API_KEY"
"#;
    let file = write_temp(yaml);

    let config = load(file.path()).expect("config dinámica sin profile debe cargar");

    let backend = &config.route[0].backend[0];
    assert_eq!(
        backend.profile, None,
        "sin `profile`, el campo queda en None"
    );
    assert_eq!(
        backend.profile(),
        Profile::Code,
        "el perfil efectivo por defecto de un backend dinámico es `code`"
    );
}

/// Scenario: Backend estático sin `model` → error nombrando ruta y backend.
#[test]
fn static_backend_without_model_is_rejected() {
    let yaml = r#"
route:
  - name: mech-writer
    backend:
      - kind: static
        base_url: "http://static-no-model.invalid/v1"
"#;
    let file = write_temp(yaml);

    let err = load(file.path()).expect_err("static sin model debe fallar");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("mech-writer"),
        "el error debe nombrar la ruta, got: {msg}"
    );
    assert!(
        msg.contains("static-no-model"),
        "el error debe nombrar el backend, got: {msg}"
    );
}

/// Scenario: Backend dinámico sin `api_key_env` → error nombrando el backend.
#[test]
fn dynamic_backend_without_api_key_env_is_rejected() {
    let yaml = r#"
route:
  - name: code
    backend:
      - kind: openrouter_free
        base_url: "http://dynamic-no-key.invalid/v1"
"#;
    let file = write_temp(yaml);

    let err = load(file.path()).expect_err("dinámico sin api_key_env debe fallar");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("dynamic-no-key"),
        "el error debe nombrar el backend afectado, got: {msg}"
    );
}

/// Scenario: Perfil inválido → error listando los perfiles admitidos.
#[test]
fn invalid_profile_is_rejected_listing_admitted_profiles() {
    let yaml = r#"
route:
  - name: code
    backend:
      - kind: openrouter_free
        profile: cualquiera
        base_url: "https://openrouter.ai/api/v1"
        api_key_env: "OPENROUTER_API_KEY"
"#;
    let file = write_temp(yaml);

    let err = load(file.path()).expect_err("perfil inválido debe fallar");
    let msg = format!("{err:#}");
    for profile in ["architect", "code", "writer"] {
        assert!(
            msg.contains(profile),
            "el error debe listar el perfil `{profile}`, got: {msg}"
        );
    }
}
