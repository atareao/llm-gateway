# Design: fix-flaky-dead-backend-tests

## Contexto

`dead_backend()` reserva un puerto efímero con `TcpListener::bind("127.0.0.1:0")`, lee la dirección y hace `drop(listener)`. El puerto queda libre y el kernel puede reasignarlo a cualquier otro `bind` del mismo proceso. Los tests de integración corren en paralelo dentro de un binario, así que otro test puede quedarse con ese puerto entre el `drop` y la petición del gateway.

Consecuencia observada: el "primario caído" responde `404` (ruta desconocida en el mock ajeno), el gateway lo propaga como 4xx no reintentable y `primary_down_fails_over_to_fallback` falla con `left: 404, right: 200`.

## Decisión

Mantener el listener enlazado durante toda la vida del test y cerrar cada conexión aceptada sin escribir respuesta:

```rust
pub async fn dead_backend() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => drop(stream),
                // Un error transitorio (ECONNABORTED, EMFILE…) no debe sacarnos
                // del bucle: si salimos, el listener se dropea, el puerto queda
                // libre y la carrera vuelve. Cedemos el turno para no girar en
                // vacío si el error es persistente.
                Err(_) => tokio::task::yield_now().await,
            }
        }
    });
    format!("http://{addr}/v1")
}
```

## Alternativas descartadas

- **Puerto fijo (p. ej. `127.0.0.1:1`)**: puede estar ocupado o requerir privilegios; no es portable en CI.
- **Listener enlazado sin aceptar**: el kernel completa el handshake por backlog y la petición se queda colgada hasta el timeout (120 s por defecto), lo que alarga los tests y cambia el escenario (timeout en vez de error de conexión).
- **`ECONNREFUSED` real**: imposible de garantizar sin la carrera que estamos arreglando.

## Consecuencias

- El error que ve `reqwest` es "connection closed before message completed" en lugar de "connection refused". Ambos son errores de transporte y el gateway los trata igual (failover al siguiente backend), así que el escenario sigue siendo válido.
- El puerto queda ocupado hasta que termina el test; el proceso de test lo libera al salir.
- `tests/common/mod.rs` necesita `#![allow(dead_code)]` porque no todos los binarios de test usan todos los helpers, y CI ejecuta `clippy --all-targets -- -D warnings`.
