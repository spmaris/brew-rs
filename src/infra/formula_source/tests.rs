use super::*;
use crate::domain::plan_error::{ApiFailure, api_failure};
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
    format!("http://{address}/formula.json")
}

fn client() -> HttpClient {
    let mut config = Config::default();
    config.downloads.use_proxy = false;
    config.downloads.timeout = 2;
    HttpClient::new(&config).unwrap()
}

#[tokio::test]
async fn typed_404_is_classified_as_not_found_for_the_planner() {
    let url = serve(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").await;
    let error = ApiError::FormulaFetch {
        name: "neovm".into(),
        source: Box::new(client().get_json::<u8>(&url).await.unwrap_err()),
    };
    let planner_error = into_planner_error(error);
    assert_eq!(api_failure(&planner_error), Some(ApiFailure::NotFound));
}

#[tokio::test]
async fn typed_503_and_unreachable_keep_their_friendly_texts() {
    let url = serve(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n").await;
    let error = into_planner_error(client().get_json::<u8>(&url).await.unwrap_err());
    assert_eq!(
        error.to_string(),
        "formulae.brew.sh returned an error (HTTP 503). Try again later."
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let error = into_planner_error(
        client()
            .get_json::<u8>(&format!("http://{address}/formula.json"))
            .await
            .unwrap_err(),
    );
    assert_eq!(
        error.to_string(),
        "Could not reach formulae.brew.sh. Check your network connection."
    );
}

#[test]
fn unclassified_failures_keep_their_typed_chain() {
    let error = into_planner_error(ApiError::FormulaFetch {
        name: "jq".into(),
        source: Box::new(ApiError::ResponseTooLarge),
    });
    assert_eq!(api_failure(&error), None);
    assert_eq!(
        format!("{error:#}"),
        "failed to fetch formula 'jq': Repository JSON exceeds the 96 MiB response limit"
    );
}
