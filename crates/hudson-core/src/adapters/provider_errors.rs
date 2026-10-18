//! Bounded, credential-safe diagnostics. Categories never authorize another call.
use serde_json::Value;
use std::io::Read;

const MAX_ERROR_BYTES: u64 = 32 * 1024;

pub(super) fn classify(status: u16, body: impl Read) -> &'static str {
    // Only these statuses need body inspection. Never echo a provider's free text,
    // identifiers, headers, or an unrecognized error code into durable state.
    if matches!(status, 400 | 413 | 422 | 429) {
        let mut bytes = Vec::new();
        if body
            .take(MAX_ERROR_BYTES + 1)
            .read_to_end(&mut bytes)
            .is_ok()
            && bytes.len() as u64 <= MAX_ERROR_BYTES
        {
            if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                let error = &value["error"];
                let code = error["code"].as_str();
                let kind = error["type"].as_str();
                if status == 429
                    && (matches!(
                        code,
                        Some(
                            "insufficient_quota"
                                | "credit_balance_exhausted"
                                | "organization_spend_limit_exceeded"
                                | "project_spend_limit_exceeded"
                                | "organization_usage_limit_exceeded"
                                | "usage_limit_exceeded"
                        )
                    ) || kind == Some("insufficient_quota"))
                {
                    return "quota_exhausted";
                }
                if matches!(status, 400 | 422) && code == Some("context_length_exceeded") {
                    return "context_limit";
                }
            }
        }
    }
    match status {
        400 | 422 => "invalid_request",
        401 => "authentication_failed",
        402 => "billing_failed",
        403 => "permission_denied",
        404 => "resource_not_found",
        408 | 504 => "request_timeout",
        409 => "request_conflict",
        413 => "request_too_large",
        429 => "rate_limited",
        500..=599 => "provider_unavailable",
        _ => "request_rejected",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn context_and_quota_codes_are_status_scoped_and_never_echoed() {
        let context =
            json!({"error":{"code":"context_length_exceeded","message":"secret"}}).to_string();
        assert_eq!(classify(400, context.as_bytes()), "context_limit");
        assert_eq!(classify(429, context.as_bytes()), "rate_limited");
        assert_eq!(classify(401, context.as_bytes()), "authentication_failed");
        for code in [
            "insufficient_quota",
            "credit_balance_exhausted",
            "organization_spend_limit_exceeded",
            "project_spend_limit_exceeded",
            "organization_usage_limit_exceeded",
            "usage_limit_exceeded",
        ] {
            let body = json!({"error":{"code":code}}).to_string();
            assert_eq!(classify(429, body.as_bytes()), "quota_exhausted");
            assert_eq!(classify(400, body.as_bytes()), "invalid_request");
        }
        let body =
            json!({"error":{"type":"insufficient_quota","code":"unrecognized-secret"}}).to_string();
        assert_eq!(classify(429, body.as_bytes()), "quota_exhausted");
    }

    #[test]
    fn malformed_oversized_and_unreadable_bodies_keep_status_diagnostics() {
        for body in [
            b"not JSON".as_slice(),
            b"null",
            b"[]",
            b"{\"error\":false}",
            b"{\"error\":{\"code\":123}}",
        ] {
            assert_eq!(classify(400, body), "invalid_request");
        }
        let mut body = json!({"error":{"code":"context_length_exceeded"}}).to_string();
        body.push_str(&" ".repeat(MAX_ERROR_BYTES as usize));
        assert_eq!(classify(400, body.as_bytes()), "invalid_request");
        struct Unreadable;
        impl Read for Unreadable {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("secret read error"))
            }
        }
        assert_eq!(classify(429, Unreadable), "rate_limited");
        assert_eq!(classify(503, Unreadable), "provider_unavailable");
    }
}
