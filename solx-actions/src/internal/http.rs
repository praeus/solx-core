//! HTTP request built-in: `http_request`.
//!
//! General-purpose HTTP fetch: any method, optional headers, optional
//! body, per-request timeout, and a structured response (status, headers,
//! body, content-type, final URL after redirects). Non-2xx responses are
//! *not* errors — the response is returned as-is and the caller decides
//! what to do with `status`. This matches how `reqwest::Client` behaves
//! by default and keeps the caller in control of the success/failure
//! definition.
//!
//! Body encoding follows the same `utf8`/`base64` convention as `file_put`
//! and `file_get`. For methods that have no semantic body (`GET`, `HEAD`),
//! reqwest itself drops the body — we don't pre-empt that.

use base64::Engine as _;
use serde_json::{json, Value};

use super::require_str;

pub(super) async fn http_request(params: &Value) -> Result<Value, String> {
    let url = require_str(params, "url")?;

    let method = params
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("GET")
        .to_ascii_uppercase();
    let timeout_secs = params
        .get("timeout_secs")
        .and_then(Value::as_u64)
        .unwrap_or(30);
    if timeout_secs == 0 {
        return Err("timeout_secs must be > 0".to_string());
    }

    let body_str = params.get("body").and_then(Value::as_str);
    let body_encoding = params
        .get("body_encoding")
        .and_then(Value::as_str)
        .unwrap_or("utf8");

    let body_bytes: Option<Vec<u8>> = match body_str {
        None => None,
        Some(s) => Some(match body_encoding {
            "utf8" => s.as_bytes().to_vec(),
            "base64" => base64::engine::general_purpose::STANDARD
                .decode(s)
                .map_err(|e| format!("invalid base64 body: {e}"))?,
            other => {
                return Err(format!(
                    "unknown body_encoding '{other}'; expected \"utf8\" or \"base64\""
                ));
            }
        }),
    };

    let mut req = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()
        .map_err(|e| format!("failed to build http client: {e}"))?
        .request(method.parse().map_err(|e| format!("invalid method: {e}"))?, url);

    if let Some(headers_val) = params.get("headers") {
        let headers_obj = headers_val
            .as_object()
            .ok_or_else(|| "headers must be an object of string -> string".to_string())?;
        let mut h = reqwest::header::HeaderMap::new();
        for (k, v) in headers_obj {
            let v_str = v
                .as_str()
                .ok_or_else(|| format!("header '{k}' must be a string"))?;
            let name = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| format!("invalid header name '{k}': {e}"))?;
            let value = reqwest::header::HeaderValue::from_str(v_str)
                .map_err(|e| format!("invalid header value for '{k}': {e}"))?;
            h.insert(name, value);
        }
        req = req.headers(h);
    }

    if let Some(b) = body_bytes {
        req = req.body(b);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("http request failed: {e}"))?;

    let status = resp.status().as_u16();
    let final_url = resp.url().to_string();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let mut headers_out = serde_json::Map::new();
    for (k, v) in resp.headers().iter() {
        if let Ok(s) = v.to_str() {
            headers_out.insert(k.as_str().to_string(), Value::String(s.to_string()));
        }
    }

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("failed to read response body: {e}"))?;
    let (body, body_encoding) = match String::from_utf8(bytes.to_vec()) {
        Ok(s) => (s, "utf8"),
        Err(_) => (
            base64::engine::general_purpose::STANDARD.encode(&bytes),
            "base64",
        ),
    };

    Ok(json!({
        "status": status,
        "url": final_url,
        "content_type": content_type,
        "headers": headers_out,
        "body": body,
        "body_encoding": body_encoding,
    }))
}
