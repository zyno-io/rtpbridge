//! WSS/HTTPS transport and HMAC authorization coverage for the control plane.

mod helpers;

use std::io::{BufReader, Cursor};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures_util::{SinkExt, StreamExt};
use helpers::test_server::TestServer;
use hmac::{Hmac, KeyInit, Mac};
use rustls::{ClientConfig, RootCertStore};
use serde_json::{Value, json};
use sha2::Sha256;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error as WebSocketError, Message, http::StatusCode};
use tokio_tungstenite::{Connector, connect_async_tls_with_config};

const CONTROL_SECRET: &[u8] = b"rtpbridge-control-test-key-must-be-at-least-32-bytes";

fn tls_connector(certificate_pem: &str) -> Connector {
    let mut roots = RootCertStore::empty();
    let mut reader = BufReader::new(Cursor::new(certificate_pem.as_bytes()));
    let certificates = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .expect("test certificate PEM should parse");
    for certificate in certificates {
        roots
            .add(certificate)
            .expect("test certificate should be accepted as a root");
    }
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Connector::Rustls(Arc::new(config))
}

fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("test clock should be after the epoch")
        .as_secs() as i64
}

fn authorization(timestamp: i64, method: &str, target: &str) -> String {
    let canonical = format!("rtpbridge-auth-v1\n{timestamp}\n{method}\n{target}");
    let mut mac = Hmac::<Sha256>::new_from_slice(CONTROL_SECRET).expect("HMAC accepts test key");
    mac.update(canonical.as_bytes());
    let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    format!("HMAC-SHA256 {timestamp}:{signature}")
}

async fn send_control_request<S>(
    ws: &mut tokio_tungstenite::WebSocketStream<S>,
    method: &str,
    params: Value,
) -> Value
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let request = json!({"id": "test", "method": method, "params": params});
    ws.send(Message::Text(request.to_string().into()))
        .await
        .expect("control request should send");
    let message = ws
        .next()
        .await
        .expect("control response should arrive")
        .expect("control response should be valid WebSocket data");
    match message {
        Message::Text(text) => {
            serde_json::from_str(&text).expect("control response should be JSON")
        }
        other => panic!("expected a text control response, received {other:?}"),
    }
}

#[tokio::test]
async fn wss_https_and_hmac_authorize_control_while_audio_keeps_its_own_token() {
    let server = TestServer::builder()
        .tls()
        .auth_hmac_secret(CONTROL_SECRET)
        .start()
        .await;
    let port = server.addr.rsplit(':').next().expect("test server port");
    let wss_url = format!("wss://localhost:{port}");
    let https_url = format!("https://localhost:{port}");
    let certificate_pem = server
        .tls_cert_pem
        .as_deref()
        .expect("TLS test server should provide its certificate");
    let connector = tls_connector(certificate_pem);

    let unauthenticated =
        connect_async_tls_with_config(&wss_url, None, false, Some(connector.clone()))
            .await
            .expect_err("control WSS must reject an absent HMAC signature");
    match unauthenticated {
        WebSocketError::Http(response) => assert_eq!(response.status(), StatusCode::UNAUTHORIZED),
        other => panic!("expected HTTP 401, received {other:?}"),
    }

    let control_authorization = authorization(unix_seconds(), "GET", "/");
    let mut control_request = wss_url
        .clone()
        .into_client_request()
        .expect("WSS URL should build a client request");
    control_request.headers_mut().insert(
        "Authorization",
        control_authorization
            .parse()
            .expect("valid Authorization value"),
    );
    let (mut control, _) =
        connect_async_tls_with_config(control_request, None, false, Some(connector.clone()))
            .await
            .expect("signed WSS control connection should succeed");

    let session = send_control_request(&mut control, "session.create", json!({})).await;
    assert!(
        session["result"]["session_id"].is_string(),
        "unexpected response: {session}"
    );
    let endpoint = send_control_request(
        &mut control,
        "endpoint.create_websocket",
        json!({"sample_rate": 8000}),
    )
    .await;
    let connect_token = endpoint["result"]["connect_token"]
        .as_str()
        .expect("WebSocket endpoint should mint an audio token");

    // The audio bearer token is its own capability: the media client never
    // receives the shared HMAC control key.
    let audio_url = format!("{wss_url}/audio/{connect_token}");
    let (mut audio, _) =
        connect_async_tls_with_config(audio_url, None, false, Some(connector.clone()))
            .await
            .expect("single-use audio token should authorize WSS audio without HMAC");
    audio
        .close(None)
        .await
        .expect("audio WebSocket should close cleanly");

    let https_client = reqwest::Client::builder()
        .add_root_certificate(
            reqwest::Certificate::from_pem(certificate_pem.as_bytes())
                .expect("test certificate should be accepted by HTTPS client"),
        )
        .build()
        .expect("HTTPS test client should build");
    let health = https_client
        .get(format!("{https_url}/health"))
        .send()
        .await
        .expect("health HTTPS request should succeed");
    assert_eq!(health.status(), StatusCode::OK);

    let sessions_url = format!("{https_url}/sessions");
    let denied = https_client
        .get(&sessions_url)
        .send()
        .await
        .expect("unsigned HTTPS request should receive a response");
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);

    let sessions_authorization = authorization(unix_seconds(), "GET", "/sessions");
    let allowed = https_client
        .get(sessions_url)
        .header("Authorization", sessions_authorization)
        .send()
        .await
        .expect("signed HTTPS request should succeed");
    assert_eq!(allowed.status(), StatusCode::OK);

    control
        .close(None)
        .await
        .expect("control WebSocket should close cleanly");
}
