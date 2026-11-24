use super::super::error::classify_parts;
use super::*;
use crate::domain::plan_error::ApiFailure;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn serve(response: &'static [u8]) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") && request.len() < 4096 {
            request.push(socket.read_u8().await.unwrap());
        }
        let _ = socket.write_all(response).await;
    });
    format!("http://{address}/metadata")
}

fn direct_client() -> HttpClient {
    let timeout = Duration::from_secs(2);
    let client = ClientBuilder::new()
        .no_proxy()
        .timeout(timeout)
        .build()
        .unwrap();
    HttpClient { client, timeout }
}

#[tokio::test]
async fn not_found_retains_http_status() {
    let url = serve(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").await;
    let error = direct_client()
        .get_json::<serde_json::Value>(&url)
        .await
        .unwrap_err();
    let ApiError::Status(source) = &error else {
        panic!("expected a status error, got {error:?}");
    };
    assert_eq!(source.status(), Some(reqwest::StatusCode::NOT_FOUND));
    assert!(error.is_not_found());
    // The compatibility wrapper keeps the typed status on the anyhow chain.
    let url = serve(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").await;
    let error = direct_client()
        .fetch_json::<serde_json::Value>(&url)
        .await
        .unwrap_err();
    assert!(is_not_found(&error));
}

#[tokio::test]
async fn streams_chunked_json() {
    let url = serve(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\n[1,\r\n2\r\n2]\r\n0\r\n\r\n",
    )
    .await;
    assert_eq!(
        direct_client().get_json::<Vec<u32>>(&url).await.unwrap(),
        vec![1, 2]
    );
}

#[tokio::test]
async fn rejects_excessive_declared_length_before_reading_body() {
    let url = serve(b"HTTP/1.1 200 OK\r\nContent-Length: 200000000\r\n\r\n").await;
    let error = direct_client()
        .get_json::<serde_json::Value>(&url)
        .await
        .unwrap_err();
    assert!(matches!(error, ApiError::ResponseTooLarge));
    assert!(error.to_string().contains("response limit"));
}

#[tokio::test]
async fn classifies_not_found_and_server_errors() {
    let url = serve(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").await;
    let error = direct_client().get_json::<u8>(&url).await.unwrap_err();
    assert_eq!(error.failure(), Some(ApiFailure::NotFound));
    let url = serve(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n").await;
    let error = direct_client().get_json::<u8>(&url).await.unwrap_err();
    assert_eq!(error.failure(), Some(ApiFailure::Server(503)));
    let url = serve(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n").await;
    let error = direct_client().get_json::<u8>(&url).await.unwrap_err();
    assert_eq!(error.failure(), None);
}

#[tokio::test]
async fn classifies_connection_failure_as_unreachable() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let error = direct_client()
        .get_json::<u8>(&format!("http://{address}/metadata"))
        .await
        .unwrap_err();
    assert!(matches!(error, ApiError::Send(_)));
    assert_eq!(error.failure(), Some(ApiFailure::Unreachable));
}

#[test]
fn classification_ignores_other_failures() {
    assert_eq!(classify_parts(Some(400), true), None);
    assert_eq!(classify_parts(None, false), None);
    assert_eq!(ApiError::ResponseTooLarge.failure(), None);
    assert!(!is_not_found(&anyhow::anyhow!("boom")));
}
