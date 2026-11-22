# Design

## Context

El gateway ya resuelve rutas como una lista ordenada de `Backend` estáticos y aplica `route_with_failover` con reintento antes del primer token (ver `src/proxy.rs`). Existe el patrón de tarea de fondo en `src/warmup.rs` y la exportación de métricas en `src/metrics.rs`. La motivación está en `proposal.md`; aquí se documenta el cómo.

## Goals / Non-Goals

**Goals:**

- Tres rutas dinámicas free por perfil (`architect`, `code`, `writer`) con un único descubrimiento del catálogo.
- Compatibilidad total con las configuraciones existentes (`kind` default `static`).
- Reutilizar el motor de failover existente, sin duplicarlo.
- Degradación a backends de pago declarados por ruta.

**Non-Goals:**

- Endpoint de administración para inspeccionar catálogo, perfiles o blacklist.
- Benchmarks propios o ranking por calidad medida.
- Descubrimiento de proveedores distintos de OpenRouter.

## Decisions

- **Un solo descubrimiento, tres clasificaciones.** Un único `discovery.rs` consulta `/models` y mantiene una caché; `classify.rs` deriva tres listas ordenadas. Alternativa descartada: tres descubridores independientes → triple tráfico, caches divergentes y más código.
- **`profile` a nivel de `Backend`, no de ruta.** Permite mezclar perfiles en una misma ruta y declarar fallbacks de pago concretos por perfil. Alternativa descartada: `profile` en la ruta → rígido y no componible.
- **Clasificación heurística con overrides.** Patrones sobre `id`/`name`/`description` + `context_length`, con exclusiones/inclusiones manuales en configuración. Alternativa descartada: listas curadas puras → se rompen cada vez que cambia el catálogo.
- **`architect` = razonamiento primero, general grande como respaldo.** Evita que la ruta quede vacía cuando no hay familias de razonamiento free con tool-calling. Alternativa descartada: solo razonamiento → ruta a menudo vacía y caídas prematuras a pago.
- **Blacklist global por modelo.** Un `429` es por combinación modelo+clave, así que la exclusión es transversal a las rutas. Alternativa descartada: blacklist por ruta → duplicación y olvido entre rutas.
- **Expansión + reutilización de `route_with_failover`.** El backend dinámico se expande a modelos concretos en su posición y el motor existente hace el resto. Alternativa descartada: motor de failover paralelo → duplicación de lógica de reintento.
- **Refresco en segundo plano con stale-while-error.** El TTL dispara refresco sin bloquear la petición y un fallo conserva la caché. Alternativa descartada: refresco síncrono en request → latencia añadida y acoplamiento a la disponibilidad de OpenRouter.

## Risks / Trade-offs

- [El tier free no incluye modelos top] → Mitigación: el fallback de pago por ruta absorbe la demanda; documentar que `architect` free es "el mejor disponible entre los free".
- [Clasificación heurística puede errar] → Mitigación: overrides manuales por configuración.
- [El catálogo free cambia con frecuencia] → Mitigación: TTL configurable y `stale-while-error` para no degradar por un fallo puntual.
- [Exceso de llamadas a `/models`] → Mitigación: TTL por defecto de 300000 ms y métricas de refresco.
- [Límites por-modelo agotan el pool free] → Mitigación: rotación round-robin + blacklist con cooldown.

## Migration Plan

- Sin migración de datos. Configs existentes siguen funcionando (`kind` default `static`).
- Activación opt-in: añadir backends `openrouter_free` a las rutas deseadas.
- Rollback: retirar los backends dinámicos y volver a los estáticos; la caché y la blacklist son en memoria.
