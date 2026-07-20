use std::collections::BTreeSet;
use std::path::Path;

use serde_json::{Map, Value, json};

use super::protocol::{Event, Request, Response};

const REDACTED: &str = "[REDACTED]";

/// Produce a useful, fail-closed representation of a control request for logs.
/// Values are only copied when both the method and field are explicitly known.
pub fn request_body(request: &Request) -> Value {
    json!({
        "id": safe_request_id(&request.id),
        "method": safe_protocol_name(&request.method),
        "params": project_known_object(&request.params, request_fields(&request.method), None),
    })
}

/// Produce a useful, fail-closed representation of a control response for logs.
pub fn response_body(method: &str, response: &Response) -> Value {
    let result = response
        .result
        .as_ref()
        .map(|value| project_known_object(value, response_fields(method), None));
    let error = response.error.as_ref().map(|error| {
        json!({
            "code": safe_token(&error.code),
            "message": {
                "redacted": true,
                "bytes": error.message.len(),
            },
        })
    });

    json!({
        "id": safe_request_id(&response.id),
        "result": result,
        "error": error,
    })
}

/// Produce a useful, fail-closed representation of a control event for logs.
pub fn event_body(event: &Event) -> Value {
    let sensitive_dtmf = event.event == "dtmf"
        && event
            .data
            .get("sensitive")
            .and_then(Value::as_bool)
            .unwrap_or(true);

    json!({
        "event": safe_protocol_name(&event.event),
        "data": project_known_object(
            &event.data,
            event_fields(&event.event),
            Some(sensitive_dtmf),
        ),
    })
}

/// Error messages can include values echoed from a failed request. Keep the
/// diagnostic size while relying on the stable error code for classification.
pub fn error_message_summary(message: &str) -> Value {
    json!({ "redacted": true, "bytes": message.len() })
}

fn project_known_object(
    value: &Value,
    allowed_fields: &[&str],
    sensitive_dtmf: Option<bool>,
) -> Value {
    let Value::Object(input) = value else {
        return type_marker(value);
    };

    let mut output = Map::new();
    for (key, value) in input {
        if allowed_fields.contains(&key.as_str()) {
            output.insert(
                safe_field_name(key),
                project_known_field(key, value, sensitive_dtmf),
            );
        } else {
            output.insert(safe_field_name(key), type_marker(value));
        }
    }
    Value::Object(output)
}

fn project_known_field(key: &str, value: &Value, sensitive_dtmf: Option<bool>) -> Value {
    match key {
        "sdp" | "sdp_offer" | "sdp_answer" => summarize_sdp(value),
        "source" => summarize_source(value),
        "file_path" => summarize_file_path(value),
        "headers" => summarize_headers(value),
        "connect_token" | "credential" | "password" | "token" => Value::String(REDACTED.into()),
        "digit" => summarize_digit(value, sensitive_dtmf.unwrap_or(true)),
        key if key.ends_with("_id") => summarize_id(value),
        "media_ip" | "endpoints" | "recordings" | "sessions" | "vad_active"
        | "fax_detect_active" | "ssrc_list" | "codecs" => summarize_array(value),
        "direction" | "type" | "endpoint_type" | "state" | "old_state" | "new_state"
        | "ice_state" | "reason" | "tone" | "codec" => summarize_enum(value),
        _ => match value {
            Value::Bool(_) | Value::Number(_) | Value::Null => value.clone(),
            Value::String(value) => json!({ "string_bytes": value.len() }),
            Value::Array(_) => summarize_array(value),
            Value::Object(value) => json!({ "object_fields": value.len() }),
        },
    }
}

fn summarize_sdp(value: &Value) -> Value {
    let Some(sdp) = value.as_str() else {
        return type_marker(value);
    };

    let mut media_types = BTreeSet::new();
    let mut codecs = BTreeSet::new();
    let mut directions = BTreeSet::new();
    let mut candidate_count = 0_u64;
    let mut has_ice = false;
    let mut has_srtp = false;

    for line in sdp.lines().map(str::trim) {
        if let Some(media) = line
            .strip_prefix("m=")
            .and_then(|line| line.split_whitespace().next())
            && matches!(media, "audio" | "video" | "application")
        {
            media_types.insert(media);
        }
        if let Some(codec) = line
            .strip_prefix("a=rtpmap:")
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.split('/').next())
        {
            codecs.insert(safe_codec(codec));
        }
        if let Some(direction) = line.strip_prefix("a=")
            && matches!(direction, "sendrecv" | "sendonly" | "recvonly" | "inactive")
        {
            directions.insert(direction);
        }
        candidate_count += u64::from(line.starts_with("a=candidate:"));
        has_ice |= line.starts_with("a=ice-ufrag:") || line.starts_with("a=ice-pwd:");
        has_srtp |= line.starts_with("a=fingerprint:")
            || line.starts_with("a=crypto:")
            || line.contains("RTP/SAVP");
    }

    json!({
        "bytes": sdp.len(),
        "media_types": media_types,
        "codecs": codecs,
        "directions": directions,
        "candidate_count": candidate_count,
        "has_ice": has_ice,
        "has_srtp": has_srtp,
    })
}

fn summarize_source(value: &Value) -> Value {
    let Some(source) = value.as_str() else {
        return type_marker(value);
    };

    if let Ok(url) = reqwest::Url::parse(source) {
        let query_keys = url
            .query_pairs()
            .map(|(key, _)| safe_query_key(&key))
            .collect::<BTreeSet<_>>();
        return json!({
            "kind": "url",
            "scheme": url.scheme(),
            "host": redact_digit_runs(url.host_str().unwrap_or(REDACTED)),
            "path": redact_digit_runs(url.path()),
            "query_keys": query_keys,
            "has_userinfo": !url.username().is_empty() || url.password().is_some(),
        });
    }

    json!({
        "kind": "file",
        "basename": redact_digit_runs(Path::new(source).file_name().and_then(|name| name.to_str()).unwrap_or(REDACTED)),
        "bytes": source.len(),
    })
}

pub(crate) fn source_summary(source: &str) -> Value {
    summarize_source(&Value::String(source.into()))
}

fn summarize_file_path(value: &Value) -> Value {
    let Some(path) = value.as_str() else {
        return type_marker(value);
    };
    json!({
        "basename": redact_digit_runs(Path::new(path).file_name().and_then(|name| name.to_str()).unwrap_or(REDACTED)),
        "bytes": path.len(),
    })
}

fn redact_digit_runs(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut digits = String::new();
    for character in value.chars().chain(std::iter::once('\0')) {
        if character.is_ascii_digit() {
            digits.push(character);
            continue;
        }
        if digits.chars().count() >= 6 {
            output.push_str(&"~".repeat(digits.chars().count()));
        } else {
            output.push_str(&digits);
        }
        digits.clear();
        if character != '\0' {
            output.push(character);
        }
    }
    output
}

fn summarize_headers(value: &Value) -> Value {
    let Some(headers) = value.as_object() else {
        return type_marker(value);
    };
    let names = headers
        .keys()
        .map(|name| safe_header_name(name))
        .collect::<BTreeSet<_>>();
    json!({ "names": names, "count": headers.len() })
}

fn summarize_digit(value: &Value, sensitive: bool) -> Value {
    let Some(digit) = value.as_str() else {
        return type_marker(value);
    };
    if sensitive || digit.chars().count() >= 6 {
        return Value::String("~".repeat(digit.chars().count()));
    }
    if digit
        .chars()
        .all(|value| matches!(value, '0'..='9' | 'A'..='D' | '#' | '*'))
    {
        Value::String(digit.into())
    } else {
        Value::String(REDACTED.into())
    }
}

fn summarize_id(value: &Value) -> Value {
    let Some(id) = value.as_str() else {
        return type_marker(value);
    };
    if uuid::Uuid::parse_str(id).is_ok() {
        Value::String(id.into())
    } else {
        Value::String(REDACTED.into())
    }
}

fn summarize_array(value: &Value) -> Value {
    match value.as_array() {
        Some(values) => json!({ "count": values.len() }),
        None => type_marker(value),
    }
}

fn summarize_enum(value: &Value) -> Value {
    let Some(value) = value.as_str() else {
        return type_marker(value);
    };
    const SAFE_VALUES: &[&str] = &[
        "active",
        "auto",
        "buffering",
        "checking",
        "completed",
        "connected",
        "connecting",
        "disconnected",
        "finished",
        "inactive",
        "new",
        "paused",
        "playing",
        "recvonly",
        "rtp",
        "sendonly",
        "sendrecv",
        "webrtc",
    ];
    if SAFE_VALUES.contains(&value) {
        Value::String(value.into())
    } else {
        Value::String(REDACTED.into())
    }
}

fn safe_codec(codec: &str) -> &'static str {
    match codec.to_ascii_lowercase().as_str() {
        "pcmu" => "PCMU",
        "pcma" => "PCMA",
        "g722" => "G722",
        "opus" => "opus",
        "telephone-event" => "telephone-event",
        _ => "other",
    }
}

fn safe_request_id(id: &str) -> Value {
    if id.len() <= 128
        && id
            .chars()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, '-' | '_'))
    {
        Value::String(id.into())
    } else {
        json!({ "redacted": true, "bytes": id.len() })
    }
}

fn safe_token(value: &str) -> Value {
    if value.len() <= 64
        && value
            .chars()
            .all(|value| value.is_ascii_uppercase() || value == '_')
    {
        Value::String(value.into())
    } else {
        Value::String(REDACTED.into())
    }
}

fn safe_query_key(value: &str) -> String {
    if value.len() <= 64
        && value.starts_with(|character: char| character.is_ascii_alphabetic() || character == '_')
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
        })
    {
        value.into()
    } else {
        REDACTED.into()
    }
}

fn safe_header_name(value: &str) -> String {
    if value.len() <= 64
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        value.to_ascii_lowercase()
    } else {
        REDACTED.into()
    }
}

fn safe_field_name(value: &str) -> String {
    if value.len() <= 64
        && value.starts_with(|character: char| character.is_ascii_alphabetic() || character == '_')
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
        })
    {
        value.into()
    } else {
        "[REDACTED_FIELD]".into()
    }
}

pub(super) fn safe_protocol_name(value: &str) -> String {
    if value.len() <= 128
        && value.starts_with(|character: char| character.is_ascii_alphabetic())
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '.'))
    {
        value.into()
    } else {
        REDACTED.into()
    }
}

fn type_marker(value: &Value) -> Value {
    let value_type = match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    };
    json!({ "redacted": true, "type": value_type })
}

fn request_fields(method: &str) -> &'static [&'static str] {
    match method {
        "session.attach" => &["session_id"],
        "endpoint.create_from_offer"
        | "endpoint.webrtc.create_from_offer"
        | "endpoint.rtp.create_from_offer" => &["sdp", "direction"],
        "endpoint.create_offer" => &["direction", "type", "srtp", "codecs"],
        "endpoint.webrtc.create_offer" => &["direction"],
        "endpoint.rtp.create_offer" => &["direction", "srtp", "codecs"],
        "endpoint.accept_answer"
        | "endpoint.webrtc.accept_answer"
        | "endpoint.rtp.accept_answer" => &["endpoint_id", "sdp", "offer_generation"],
        "endpoint.accept_offer" | "endpoint.webrtc.accept_offer" => &["endpoint_id", "sdp"],
        "endpoint.remove"
        | "endpoint.ice_restart"
        | "endpoint.webrtc.ice_restart"
        | "endpoint.srtp_rekey"
        | "endpoint.rtp.srtp_rekey"
        | "endpoint.file.pause"
        | "endpoint.file.resume"
        | "vad.stop"
        | "fax_detect.start"
        | "fax_detect.stop" => &["endpoint_id"],
        "endpoint.dtmf.inject" => &["endpoint_id", "digit", "duration_ms", "volume"],
        "endpoint.dtmf.set_sensitive" => &["endpoint_id", "enabled"],
        "recording.start" => &["endpoint_id", "file_path"],
        "recording.stop" => &["recording_id"],
        "vad.start" => &["endpoint_id", "silence_interval_ms", "speech_threshold"],
        "endpoint.create_with_file" => &[
            "source",
            "start_ms",
            "loop_count",
            "cache_ttl_secs",
            "shared",
            "timeout_ms",
            "headers",
            "gain_db",
        ],
        "endpoint.file.seek" => &["endpoint_id", "position_ms"],
        "endpoint.create_tone" => &["tone", "frequency", "duration_ms"],
        "endpoint.create_websocket" => &["direction", "sample_rate", "flush_ms"],
        "endpoint.update_direction" => &["endpoint_id", "direction"],
        "endpoint.update_remote_sdp" | "endpoint.rtp.reinvite" => &["endpoint_id", "sdp"],
        "stats.subscribe" => &["interval_ms", "include_diagnostics"],
        "endpoint.transfer" => &["endpoint_id", "target_session_id"],
        "session.bridge" => &["target_session_id", "direction"],
        "session.create" | "session.destroy" | "session.info" | "session.list" | "server.info"
        | "stats.unsubscribe" => &[],
        _ => &[],
    }
}

fn response_fields(method: &str) -> &'static [&'static str] {
    match method {
        "session.create" | "session.attach" => &["session_id"],
        "session.info" => &[
            "session_id",
            "state",
            "created_at",
            "endpoints",
            "recordings",
            "vad_active",
            "fax_detect_active",
        ],
        "session.list" => &["sessions"],
        "server.info" => &["hostname", "version", "media_ip"],
        "endpoint.create_from_offer"
        | "endpoint.webrtc.create_from_offer"
        | "endpoint.rtp.create_from_offer" => &["endpoint_id", "sdp_answer"],
        "endpoint.create_offer" | "endpoint.webrtc.create_offer" | "endpoint.rtp.create_offer" => {
            &["endpoint_id", "sdp_offer"]
        }
        "endpoint.accept_offer" | "endpoint.webrtc.accept_offer" => &["sdp_answer"],
        "endpoint.ice_restart" | "endpoint.webrtc.ice_restart" => {
            &["sdp_offer", "offer_generation"]
        }
        "endpoint.srtp_rekey" | "endpoint.rtp.srtp_rekey" => &["sdp"],
        "endpoint.create_with_file" | "endpoint.create_tone" => &["endpoint_id", "tone"],
        "endpoint.create_websocket" => &["endpoint_id", "connect_token"],
        "recording.start" => &["recording_id"],
        "recording.stop" => &["file_path", "duration_ms", "packets", "dropped_packets"],
        "endpoint.transfer" => &["endpoint_id", "target_session_id"],
        "session.bridge" => &["endpoint_id", "target_endpoint_id"],
        _ => &[],
    }
}

fn event_fields(event: &str) -> &'static [&'static str] {
    match event {
        "dtmf" => &["endpoint_id", "digit", "duration_ms", "sensitive"],
        "endpoint.state_changed" => &["endpoint_id", "old_state", "new_state"],
        "endpoint.ice_state_changed" => &["endpoint_id", "ice_state"],
        "endpoint.file.finished" => &["endpoint_id", "reason", "error"],
        "endpoint.tone.finished" => &["endpoint_id"],
        "recording.stopped" => &[
            "recording_id",
            "file_path",
            "duration_ms",
            "packets",
            "dropped_packets",
            "reason",
        ],
        "session.orphaned" => &["timeout_remaining_ms"],
        "endpoint.media_timeout" => &["endpoint_id", "duration_ms"],
        "vad.speech_started" => &["endpoint_id"],
        "vad.silence" => &["endpoint_id", "silence_duration_ms"],
        "fax.detected" => &["endpoint_id"],
        "endpoint.ws.connected" | "endpoint.ws.disconnected" | "endpoint.ws.connect_timeout" => {
            &["endpoint_id"]
        }
        "endpoint.transferred_out" => &["endpoint_id", "target_session_id"],
        "endpoint.transferred_in" => &[
            "endpoint_id",
            "source_session_id",
            "endpoint_type",
            "direction",
            "state",
        ],
        "endpoint.rtcp_bye" => &["endpoint_id", "ssrc_list", "reason"],
        "session.idle_timeout" => &["session_id", "idle_timeout_secs"],
        "session.empty_timeout" => &["session_id", "empty_timeout_secs"],
        "events.dropped" => &["count"],
        "stats" => &["endpoints"],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::control::protocol::{ErrorInfo, Event, Request, Response};

    const CARD: &str = "4111111111111111";
    const PASSWORD: &str = "turn-password-secret";
    const SRTP_KEY: &str = "inline:super-secret-srtp-key";

    fn serialized(value: Value) -> String {
        serde_json::to_string(&value).unwrap()
    }

    #[test]
    fn request_scrubs_sdp_secrets_and_unknown_fields() {
        let request = Request {
            id: "request-1".into(),
            method: "endpoint.rtp.create_from_offer".into(),
            params: json!({
                "direction": "sendrecv",
                "sdp": format!("v=0\r\nm=audio 10000 RTP/SAVP 0\r\na=ice-pwd:{PASSWORD}\r\na=crypto:1 AES_CM_128_HMAC_SHA1_80 {SRTP_KEY}\r\na=rtpmap:0 PCMU/8000\r\n"),
                "credit_card": CARD,
            }),
        };

        let output = serialized(request_body(&request));
        assert!(!output.contains(CARD));
        assert!(!output.contains(PASSWORD));
        assert!(!output.contains(SRTP_KEY));
        assert!(output.contains("candidate_count"));
        assert!(output.contains("credit_card"));
        assert!(output.contains("redacted"));
    }

    #[test]
    fn source_scrubs_credentials_headers_and_query_values() {
        let request = Request {
            id: "request-2".into(),
            method: "endpoint.create_with_file".into(),
            params: json!({
                "source": format!("https://user:{PASSWORD}@media{CARD}.example.test/{CARD}/hold.wav?token={CARD}&cache=yes"),
                "headers": { "Authorization": PASSWORD, "X-Card": CARD },
            }),
        };

        let output = serialized(request_body(&request));
        assert!(!output.contains(CARD));
        assert!(!output.contains(PASSWORD));
        assert!(output.contains("media~~~~~~~~~~~~~~~~.example.test"));
        assert!(output.contains("authorization"));
        assert!(output.contains("token"));
    }

    #[test]
    fn sensitive_dtmf_uses_tildes_and_normal_dtmf_remains_visible() {
        let sensitive = Event::new(
            "dtmf",
            json!({ "endpoint_id": uuid::Uuid::new_v4(), "digit": "12#", "sensitive": true }),
        );
        let normal = Event::new(
            "dtmf",
            json!({ "endpoint_id": uuid::Uuid::new_v4(), "digit": "5", "sensitive": false }),
        );
        let unexpectedly_long = Event::new(
            "dtmf",
            json!({ "endpoint_id": uuid::Uuid::new_v4(), "digit": CARD, "sensitive": false }),
        );

        let sensitive_output = serialized(event_body(&sensitive));
        let normal_output = serialized(event_body(&normal));
        let unexpectedly_long_output = serialized(event_body(&unexpectedly_long));
        assert!(!sensitive_output.contains("12#"));
        assert!(sensitive_output.contains("~~~"));
        assert!(normal_output.contains("\"digit\":\"5\""));
        assert!(!unexpectedly_long_output.contains(CARD));
        assert!(unexpectedly_long_output.contains(&"~".repeat(CARD.len())));
    }

    #[test]
    fn unknown_methods_and_error_messages_never_copy_values() {
        let mut params = serde_json::Map::new();
        params.insert("state".into(), Value::String(CARD.into()));
        params.insert("password".into(), Value::String(PASSWORD.into()));
        params.insert(CARD.into(), Value::String(PASSWORD.into()));
        let request = Request {
            id: "request-3".into(),
            method: "future.method".into(),
            params: Value::Object(params),
        };
        let response = Response {
            id: "request-3".into(),
            result: None,
            error: Some(ErrorInfo {
                code: "INVALID_PARAMS".into(),
                message: format!("card {CARD}; password {PASSWORD}"),
            }),
        };

        let output = format!(
            "{}{}",
            serialized(request_body(&request)),
            serialized(response_body("future.method", &response)),
        );
        assert!(!output.contains(CARD));
        assert!(!output.contains(PASSWORD));
        assert!(output.contains("INVALID_PARAMS"));
    }

    #[test]
    fn websocket_connect_tokens_are_redacted() {
        let response = Response {
            id: "request-4".into(),
            result: Some(json!({
                "endpoint_id": uuid::Uuid::new_v4(),
                "connect_token": PASSWORD,
            })),
            error: None,
        };

        let output = serialized(response_body("endpoint.create_websocket", &response));
        assert!(!output.contains(PASSWORD));
        assert!(output.contains(REDACTED));
    }
}
