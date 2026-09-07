//! Browser session authentication for the control-panel pages.
//!
//! Agents authenticate to `/v1` with a bearer token. Browsers must never see
//! that token, so pages are protected by a signed, stateless session cookie
//! instead. The cookie value is `{expires_ms}.{hex(HMAC-SHA256(api_key,
//! "gateway-ui-session:{expires_ms}"))}`: it carries no server state, survives
//! restarts, and is invalidated automatically if the API key changes.
//!
//! Same-origin XHR from the pages reuses the cookie. To keep that CSRF-safe the
//! bearer middleware only honours the cookie when the request also carries the
//! custom `X-Gateway-UI` header, which cross-origin pages cannot add without a
//! CORS preflight the gateway never grants.

use axum::http::HeaderMap;
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Cookie that carries the browser session.
pub const COOKIE_NAME: &str = "gw_session";
/// Header the page runtime adds to every same-origin API call.
pub const UI_HEADER: &str = "x-gateway-ui";
/// Session lifetime: 30 days.
pub const SESSION_TTL_MS: i64 = 30 * 24 * 60 * 60 * 1000;

fn sign(api_key: &str, expires_ms: i64) -> String {
    let mut mac =
        HmacSha256::new_from_slice(api_key.as_bytes()).expect("HMAC accepts any key length");
    mac.update(format!("gateway-ui-session:{expires_ms}").as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Mint a session token that expires at `expires_ms` (epoch milliseconds).
pub fn session_token(api_key: &str, expires_ms: i64) -> String {
    format!("{expires_ms}.{}", sign(api_key, expires_ms))
}

/// Validate a session token against the current key and clock.
pub fn verify_session_token(api_key: &str, token: &str, now_ms: i64) -> bool {
    let Some((expires, sig)) = token.split_once('.') else {
        return false;
    };
    let Ok(expires_ms) = expires.parse::<i64>() else {
        return false;
    };
    if expires_ms <= now_ms {
        return false;
    }
    let expected = sign(api_key, expires_ms);
    // Constant-time compare via the MAC crate would need the raw bytes; the
    // hex strings are fixed-length so a length check plus byte fold suffices.
    if expected.len() != sig.len() {
        return false;
    }
    expected
        .bytes()
        .zip(sig.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

/// Extract a named cookie from the request headers.
pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all("cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|line| line.split(';'))
        .filter_map(|pair| {
            let (k, v) = pair.trim().split_once('=')?;
            (k.trim() == name).then(|| v.trim().to_string())
        })
        .next()
}

/// True when the request carries a valid session cookie.
pub fn has_valid_session(api_key: &str, headers: &HeaderMap, now_ms: i64) -> bool {
    cookie_value(headers, COOKIE_NAME)
        .map(|token| verify_session_token(api_key, &token, now_ms))
        .unwrap_or(false)
}

/// True when the request is same-origin page XHR: valid cookie plus the
/// custom header the page runtime sets.
pub fn has_valid_ui_xhr(api_key: &str, headers: &HeaderMap, now_ms: i64) -> bool {
    headers.contains_key(UI_HEADER) && has_valid_session(api_key, headers, now_ms)
}

/// Build the `Set-Cookie` header value for a fresh session.
pub fn set_cookie_header(token: &str, secure: bool, max_age_secs: i64) -> String {
    let mut value =
        format!("{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age_secs}");
    if secure {
        value.push_str("; Secure");
    }
    value
}

/// `Set-Cookie` value that clears the session.
pub fn clear_cookie_header() -> String {
    format!("{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0")
}

/// Whether the original request arrived over HTTPS (directly or via a proxy
/// that sets `X-Forwarded-Proto`). Used to decide the cookie's `Secure` flag.
pub fn request_is_https(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(',')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("https")
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn token_round_trips_and_expires() {
        let token = session_token("k", 10_000);
        assert!(verify_session_token("k", &token, 9_999));
        assert!(!verify_session_token("k", &token, 10_000));
        assert!(!verify_session_token("other", &token, 1));
        assert!(!verify_session_token("k", "garbage", 1));
        assert!(!verify_session_token("k", "10000.deadbeef", 1));
    }

    #[test]
    fn cookie_parsing_handles_multiple_pairs() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "cookie",
            HeaderValue::from_static("a=1; gw_session=tok.sig ;b=2"),
        );
        assert_eq!(
            cookie_value(&headers, COOKIE_NAME).as_deref(),
            Some("tok.sig")
        );
        assert_eq!(cookie_value(&headers, "b").as_deref(), Some("2"));
        assert_eq!(cookie_value(&headers, "missing"), None);
    }

    #[test]
    fn ui_xhr_requires_header_and_cookie() {
        let token = session_token("k", i64::MAX);
        let mut headers = HeaderMap::new();
        headers.insert(
            "cookie",
            HeaderValue::from_str(&format!("{COOKIE_NAME}={token}")).unwrap(),
        );
        assert!(has_valid_session("k", &headers, 0));
        assert!(!has_valid_ui_xhr("k", &headers, 0));
        headers.insert(UI_HEADER, HeaderValue::from_static("1"));
        assert!(has_valid_ui_xhr("k", &headers, 0));
    }

    #[test]
    fn https_detection_reads_forwarded_proto() {
        let mut headers = HeaderMap::new();
        assert!(!request_is_https(&headers));
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https, http"));
        assert!(request_is_https(&headers));
        assert!(set_cookie_header("t", true, 5).ends_with("; Secure"));
        assert!(!set_cookie_header("t", false, 5).contains("Secure"));
    }
}
