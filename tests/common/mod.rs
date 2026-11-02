//! Helpers compartidos por los tests de integración.
//!
//! `#![allow(dead_code)]` porque cada binario de test compila este módulo
//! completo y no todos usan todos los helpers; CI ejecuta
//! `clippy --all-targets -- -D warnings`.
#![allow(dead_code)]

use tokio::net::TcpListener;

/// Devuelve una `base_url` a un backend caído.
///
/// El listener se mantiene enlazado durante toda la vida del test y cierra
/// cada conexión aceptada sin responder. Así el puerto **nunca** queda libre:
/// si lo liberásemos con `drop(listener)`, otro test que corre en paralelo
/// podría enlazarlo y el "primario caído" reviviría respondiendo `404`, lo que
/// convertía `primary_down_fails_over_to_fallback` en un test flaky.
///
/// El cliente ve un error de transporte (conexión cerrada sin respuesta), que
/// es justo lo que el gateway debe tratar como backend no disponible.
pub async fn dead_backend() -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
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
