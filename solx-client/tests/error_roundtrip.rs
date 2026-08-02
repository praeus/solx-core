//! Confirms a non-2xx response from a server surfaces the *exact*
//! `SolxError` variant through a `Remote*Manager` call, not a collapsed
//! generic one — the whole point of `SolxError`'s tagged wire format.

use axum::extract::Json;
use axum::http::StatusCode;
use axum::routing::post;
use axum::Router;
use solx_client::RemoteTypeManager;
use solx_surface::error::SolxError;
use solx_surface::managers::TypeManager;

async fn spawn_test_server(status: StatusCode, error: SolxError) -> String {
    let router = Router::new().route(
        "/types/get",
        post(move || {
            let error = error.clone();
            async move { (status, Json(error)) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn not_found_variant_round_trips() {
    let base_url = spawn_test_server(StatusCode::NOT_FOUND, SolxError::NotFound("thing".into())).await;
    let client = RemoteTypeManager::new(base_url, "unused-token");

    let err = client.get("/a", "b").await.unwrap_err();
    assert_eq!(err, SolxError::NotFound("thing".into()));
}

#[tokio::test]
async fn validation_variant_round_trips() {
    let base_url = spawn_test_server(
        StatusCode::UNPROCESSABLE_ENTITY,
        SolxError::Validation("bad shape".into()),
    )
    .await;
    let client = RemoteTypeManager::new(base_url, "unused-token");

    let err = client.get("/a", "b").await.unwrap_err();
    assert_eq!(err, SolxError::Validation("bad shape".into()));
    // Specifically not collapsed into a generic variant.
    assert_ne!(err, SolxError::Other("bad shape".into()));
}
