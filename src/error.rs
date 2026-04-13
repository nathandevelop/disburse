//! JSON-RPC error codes and response builder.
//!
//! Codes in the -32000..-32099 range are "server error" per the JSON-RPC 2.0
//! spec (app-defined). We use them to let clients distinguish proxy-layer
//! failures from upstream-layer failures without parsing the message string.

use serde_json::{json, Value};

/// solmux-specific JSON-RPC error codes.
#[derive(Debug, Clone, Copy)]
pub enum ErrorCode {
    /// -32600: the client sent something we couldn't even parse as JSON-RPC.
    InvalidRequest,
    /// -32001: selected upstream refused connection / DNS failure / TLS error.
    UpstreamUnreachable,
    /// -32002: sendTransaction rejected because its blockhash is stale/unknown.
    BlockhashStale,
    /// -32003: all upstreams attempted, all failed.
    AllUpstreamsExhausted,
    /// -32004: upstream didn't respond within the configured timeout.
    UpstreamTimeout,
    /// -32005: upstream returned a non-JSON or malformed response body.
    UpstreamBadResponse,
    /// -32006: upstream response body exceeded `max_response_body_bytes`.
    ResponseTooLarge,
    /// -32008: request exceeded the overall `retries.deadline_ms`.
    DeadlineExceeded,
    /// -32603: catch-all internal error.
    Internal,
}

impl ErrorCode {
    pub fn as_i32(self) -> i32 {
        match self {
            Self::InvalidRequest => -32600,
            Self::UpstreamUnreachable => -32001,
            Self::BlockhashStale => -32002,
            Self::AllUpstreamsExhausted => -32003,
            Self::UpstreamTimeout => -32004,
            Self::UpstreamBadResponse => -32005,
            Self::ResponseTooLarge => -32007,
            Self::DeadlineExceeded => -32008,
            Self::Internal => -32603,
        }
    }
}

/// Build a JSON-RPC 2.0 error response.
pub fn jsonrpc_error(id: Value, code: ErrorCode, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code.as_i32(), "message": message }
    })
}

/// Redact `api-key` / `apikey` / `token` query parameters from a URL so it's
/// safe to log. Falls back to the raw string if the URL doesn't parse.
pub fn redact_url(raw: &str) -> String {
    const SECRET_KEYS: &[&str] = &[
        "api-key",
        "api_key",
        "apikey",
        "token",
        "access_token",
        "auth",
        "key",
    ];
    let Ok(mut parsed) = url::Url::parse(raw) else {
        return raw.to_string();
    };
    let pairs: Vec<(String, String)> = parsed
        .query_pairs()
        .map(|(k, v)| {
            let lower = k.to_ascii_lowercase();
            let val = if SECRET_KEYS.iter().any(|s| lower == *s) {
                "REDACTED".to_string()
            } else {
                v.into_owned()
            };
            (k.into_owned(), val)
        })
        .collect();
    if pairs.is_empty() {
        return parsed.to_string();
    }
    parsed.query_pairs_mut().clear();
    for (k, v) in pairs {
        parsed.query_pairs_mut().append_pair(&k, &v);
    }
    parsed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_api_key_query_param() {
        assert_eq!(
            redact_url("https://mainnet.helius-rpc.com/?api-key=supersecret"),
            "https://mainnet.helius-rpc.com/?api-key=REDACTED"
        );
    }

    #[test]
    fn redacts_mixed_case_key_names() {
        let out = redact_url("https://x.example/?API_KEY=a&token=b&foo=bar");
        assert!(out.contains("API_KEY=REDACTED"));
        assert!(out.contains("token=REDACTED"));
        assert!(out.contains("foo=bar"));
    }

    #[test]
    fn leaves_url_without_query_alone() {
        assert_eq!(
            redact_url("https://api.example.com/rpc"),
            "https://api.example.com/rpc"
        );
    }

    #[test]
    fn falls_back_on_unparseable() {
        assert_eq!(redact_url("not a url"), "not a url");
    }
}
