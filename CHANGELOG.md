# Changelog
## [0.3.2] - 2026-11-07

### Miscellaneous Tasks

- *(publish)* Renombra el paquete a llm-gateway-rs y añade metadata para crates.io (#15)
## [0.3.1] - 2026-11-07

### Miscellaneous Tasks

- *(release)* Automatiza versionado y publicación (vampus + git-cliff + Actions) (#12)
- *(release)* Workflow único sin GH_PAT (#13)
## [0.3.0] - 2026-11-07

### Documentation

- *(warmup)* Marcar tarea 5.1 verificada con Ollama real (#9)

### Features

- *(warmup)* Keep-alive de backends con prewarm y heartbeat (#8)
- *(warmup)* Hardening del keep-alive (#10)
## [0.2.0] - 2026-11-02

### Documentation

- *(openspec)* Documentar y archivar add-non-stream-default-timeout
- *(openspec)* Propone fix-flaky-dead-backend-tests
- *(openspec)* Archiva fix-flaky-dead-backend-tests
- *(openspec)* Propone add-observability
- *(openspec)* Archiva add-observability

### Features

- *(proxy)* Timeout total por defecto (120 s) en peticiones no-stream
- *(observability)* Métricas Prometheus, logs correlacionados y x-request-id

### Miscellaneous Tasks

- *(openspec)* Marca la tarea de PR como completada

### Testing

- *(proxy)* Cubrir timeout por defecto en no-stream y de primer byte
- *(harness)* Elimina la carrera de puertos en dead_backend
## [0.1.0] - 2026-10-31

### Bug Fixes

- *(gateway)* Timeout de primer byte en streaming y test de corte a mitad de stream

### Documentation

- *(gateway)* Cambia la config de rutas de TOML a YAML (serde-saphyr)
- *(gateway)* Archiva el change add-llm-gateway y consolida la spec

### Features

- *(gateway)* Config YAML de rutas con validación (TDD rojo-verde)
- *(gateway)* Endpoint /v1/chat/completions no-streaming con reescritura de model (TDD)
- *(gateway)* Failover antes del primer token con cabecera x-gateway-backend (TDD)
- *(gateway)* Streaming SSE y endpoints /v1/models y /healthz (TDD)
- *(gateway)* Secretos por entorno, integración, refactor y docs (TDD)

### Miscellaneous Tasks

- Scaffold del repo y change OpenSpec add-llm-gateway
- Añade LICENSE MIT, README y andamiaje Git Flow + CI

### Testing

- *(gateway)* Verificación e2e con Ollama real y failover
