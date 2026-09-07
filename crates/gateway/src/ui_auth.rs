//! Browser authentication for the control-panel pages.
//!
//! Agents authenticate to `/v1` with the shared bearer token. People sign in
//! with a username and password (see `db::users`); the session is a signed,
//! stateless cookie naming the user:
//!
//! `{user_id}.{expires_ms}.{hex(HMAC-SHA256(api_key,
//! "gateway-ui-session:{user_id}:{expires_ms}:{session_epoch}"))}`
//!
//! The signature binds the user's `session_epoch`, so changing a password or
//! disabling the account invalidates every outstanding cookie without server
//! state. Changing the API key does the same for everyone.
//!
//! Same-origin XHR from the pages reuses the cookie. To keep that CSRF-safe the
//! bearer middleware only honours the cookie when the request also carries the
//! custom `X-Gateway-UI` header, which cross-origin pages cannot add without a
//! CORS preflight the gateway never grants. When a cookie is accepted, the
//! middleware stamps `X-Gateway-User` / `X-Gateway-User-Role` onto the request
//! (replacing anything the client sent) so handlers can attribute actions.

use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use axum::http::HeaderMap;
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::db::User;

type HmacSha256 = Hmac<Sha256>;

/// Cookie that carries the browser session (HttpOnly).
pub const COOKIE_NAME: &str = "gw_session";
/// Readable cookie with the display name, for the header "Signed in as".
pub const WHOAMI_COOKIE: &str = "gw_user";
/// Header the page runtime adds to every same-origin API call.
pub const UI_HEADER: &str = "x-gateway-ui";
/// Headers the middleware stamps on authenticated browser requests.
pub const USER_HEADER: &str = "x-gateway-user";
pub const USER_ROLE_HEADER: &str = "x-gateway-user-role";
/// Session lifetime: 30 days.
pub const SESSION_TTL_MS: i64 = 30 * 24 * 60 * 60 * 1000;
pub const MIN_PASSWORD_LEN: usize = 8;

/// Who a browser request belongs to, as resolved by the middleware.
#[derive(Debug, Clone)]
pub struct SessionUser {
    pub id: i64,
    pub username: String,
    pub display_name: String,
    pub role: String,
}

impl SessionUser {
    pub fn is_admin(&self) -> bool {
        self.role == crate::db::USER_ROLE_ADMIN
    }
}

// ── Passwords ────────────────────────────────────────────────────────────────

/// Hash a password with argon2id and a fresh random salt (PHC string).
pub fn hash_password(password: &str) -> anyhow::Result<String> {
    if password.chars().count() < MIN_PASSWORD_LEN {
        anyhow::bail!("password must be at least {MIN_PASSWORD_LEN} characters");
    }
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow::anyhow!("hash password: {e}"))
}

/// Constant-time verification against a stored PHC string.
pub fn verify_password(password: &str, phc: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(phc) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

// ── Session tokens ───────────────────────────────────────────────────────────

fn sign(api_key: &str, user_id: i64, expires_ms: i64, epoch: i64) -> String {
    let mut mac =
        HmacSha256::new_from_slice(api_key.as_bytes()).expect("HMAC accepts any key length");
    mac.update(format!("gateway-ui-session:{user_id}:{expires_ms}:{epoch}").as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Mint a session token for `user` that expires at `expires_ms`.
pub fn session_token(api_key: &str, user: &User, expires_ms: i64) -> String {
    format!(
        "{}.{expires_ms}.{}",
        user.id,
        sign(api_key, user.id, expires_ms, user.session_epoch)
    )
}

/// Parse the `user_id` out of a token without verifying it (the caller then
/// loads the user and calls [`verify_session_token`]).
pub fn token_user_id(token: &str) -> Option<i64> {
    token.split('.').next()?.parse().ok()
}

/// Validate a token against the user it names, the key, and the clock.
pub fn verify_session_token(api_key: &str, user: &User, token: &str, now_ms: i64) -> bool {
    let mut parts = token.splitn(3, '.');
    let (Some(uid), Some(expires), Some(sig)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let Ok(uid) = uid.parse::<i64>() else {
        return false;
    };
    let Ok(expires_ms) = expires.parse::<i64>() else {
        return false;
    };
    if uid != user.id || expires_ms <= now_ms || !user.is_active() {
        return false;
    }
    let expected = sign(api_key, user.id, expires_ms, user.session_epoch);
    expected.len() == sig.len()
        && expected
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

/// The session token from the request, if any.
pub fn session_cookie(headers: &HeaderMap) -> Option<String> {
    cookie_value(headers, COOKIE_NAME)
}

/// True when the request is same-origin page XHR (custom header present).
pub fn is_ui_xhr(headers: &HeaderMap) -> bool {
    headers.contains_key(UI_HEADER)
}

/// The user the middleware stamped on this request, if it came from a browser
/// session. Requests authenticated with the bearer key have none.
pub fn request_user(headers: &HeaderMap) -> Option<SessionUser> {
    let username = headers.get(USER_HEADER)?.to_str().ok()?.to_string();
    let role = headers
        .get(USER_ROLE_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or(crate::db::USER_ROLE_MEMBER)
        .to_string();
    let (id, username, display_name) = match username.split_once(':') {
        // Encoded as "id:username:display name".
        Some((id, rest)) => {
            let (name, display) = rest.split_once(':').unwrap_or((rest, rest));
            (
                id.parse().unwrap_or(0),
                name.to_string(),
                display.to_string(),
            )
        }
        None => (0, username.clone(), username),
    };
    Some(SessionUser {
        id,
        username,
        display_name,
        role,
    })
}

/// Header value encoding for [`request_user`].
pub fn user_header_value(user: &User) -> String {
    let clean =
        |s: &str| -> String { s.chars().filter(|c| !c.is_control() && *c != ':').collect() };
    format!(
        "{}:{}:{}",
        user.id,
        clean(&user.username),
        clean(&user.display_name)
    )
}

// ── Cookies ──────────────────────────────────────────────────────────────────

/// `Set-Cookie` values for a fresh session: the HttpOnly token and the
/// readable display-name cookie the header uses.
pub fn set_cookie_headers(token: &str, display_name: &str, secure: bool) -> Vec<String> {
    let max_age = SESSION_TTL_MS / 1000;
    let secure_attr = if secure { "; Secure" } else { "" };
    let display: String = display_name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '.' | '_' | '-'))
        .take(60)
        .collect();
    vec![
        format!(
            "{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{secure_attr}"
        ),
        format!(
            "{WHOAMI_COOKIE}={}; Path=/; SameSite=Strict; Max-Age={max_age}{secure_attr}",
            display.replace(' ', "+")
        ),
    ]
}

/// `Set-Cookie` values that clear the session.
pub fn clear_cookie_headers() -> Vec<String> {
    vec![
        format!("{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0"),
        format!("{WHOAMI_COOKIE}=; Path=/; SameSite=Strict; Max-Age=0"),
    ]
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

    fn user(id: i64, epoch: i64, disabled: bool) -> User {
        User {
            id,
            username: "will".into(),
            display_name: "Will H".into(),
            role: "admin".into(),
            password_hash: String::new(),
            session_epoch: epoch,
            disabled_at: disabled.then_some(1),
            created_at: 0,
            last_login_at: None,
        }
    }

    #[test]
    fn password_hash_round_trips_and_enforces_length() {
        assert!(hash_password("short").is_err());
        let phc = hash_password("correct horse battery").unwrap();
        assert!(phc.starts_with("$argon2id$"));
        assert!(verify_password("correct horse battery", &phc));
        assert!(!verify_password("wrong", &phc));
        assert!(!verify_password("x", "not-a-phc"));
    }

    #[test]
    fn token_binds_user_expiry_epoch_and_key() {
        let u = user(7, 1, false);
        let token = session_token("k", &u, 10_000);
        assert_eq!(token_user_id(&token), Some(7));
        assert!(verify_session_token("k", &u, &token, 9_999));
        assert!(!verify_session_token("k", &u, &token, 10_000), "expired");
        assert!(!verify_session_token("other", &u, &token, 1), "key");
        assert!(
            !verify_session_token("k", &user(8, 1, false), &token, 1),
            "user"
        );
        assert!(
            !verify_session_token("k", &user(7, 2, false), &token, 1),
            "epoch"
        );
        assert!(
            !verify_session_token("k", &user(7, 1, true), &token, 1),
            "disabled"
        );
        assert!(!verify_session_token("k", &u, "garbage", 1));
    }

    #[test]
    fn cookie_parsing_and_user_header_round_trip() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "cookie",
            HeaderValue::from_static("a=1; gw_session=7.1.sig ;b=2"),
        );
        assert_eq!(session_cookie(&headers).as_deref(), Some("7.1.sig"));
        assert!(!is_ui_xhr(&headers));
        headers.insert(UI_HEADER, HeaderValue::from_static("1"));
        assert!(is_ui_xhr(&headers));

        let u = user(7, 1, false);
        headers.insert(
            USER_HEADER,
            HeaderValue::from_str(&user_header_value(&u)).unwrap(),
        );
        headers.insert(USER_ROLE_HEADER, HeaderValue::from_static("admin"));
        let su = request_user(&headers).unwrap();
        assert_eq!(su.id, 7);
        assert_eq!(su.username, "will");
        assert_eq!(su.display_name, "Will H");
        assert!(su.is_admin());
    }

    #[test]
    fn cookie_headers_are_well_formed() {
        let set = set_cookie_headers("t", "Will H", true);
        assert!(set[0].starts_with("gw_session=t;"));
        assert!(set[0].contains("HttpOnly") && set[0].ends_with("; Secure"));
        assert!(set[1].starts_with("gw_user=Will+H;") && !set[1].contains("HttpOnly"));
        let cleared = clear_cookie_headers();
        assert!(cleared.iter().all(|c| c.contains("Max-Age=0")));
        let mut headers = HeaderMap::new();
        assert!(!request_is_https(&headers));
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https, http"));
        assert!(request_is_https(&headers));
    }
}
