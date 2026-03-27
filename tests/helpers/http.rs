#![allow(dead_code)]

use super::timing;

/// HTTP GET request returning parsed JSON.
pub async fn http_get(addr: &str, path: &str) -> serde_json::Value {
    let url = format!("http://{addr}{path}");
    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .timeout(timing::scaled_ms(5000))
        .send()
        .await
        .unwrap_or_else(|e| panic!("HTTP GET {path} failed: {e}"));
    let text = resp.text().await.unwrap();
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("not JSON: {e}\n{text}"))
}

/// HTTP GET request returning status code and raw text body.
pub async fn http_get_text(addr: &str, path: &str) -> (u16, String) {
    let url = format!("http://{addr}{path}");
    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .timeout(timing::scaled_ms(5000))
        .send()
        .await
        .unwrap_or_else(|e| panic!("HTTP GET {path} failed: {e}"));
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap();
    (status, text)
}

/// HTTP DELETE request returning status code and parsed JSON body.
pub async fn http_delete(addr: &str, path: &str) -> (u16, serde_json::Value) {
    let url = format!("http://{addr}{path}");
    let client = reqwest::Client::new();
    let resp = client
        .delete(&url)
        .timeout(timing::scaled_ms(5000))
        .send()
        .await
        .unwrap_or_else(|e| panic!("HTTP DELETE {path} failed: {e}"));
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap();
    let json = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("HTTP DELETE response not JSON: {e}\n{text}"));
    (status, json)
}
