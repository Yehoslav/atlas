use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::HeaderValue;
use getrandom::fill;
use subtle::ConstantTimeEq;

const COOKIE: &str = "atlas_admin";
const SESSION_SECONDS: u32 = 8 * 60 * 60;

/// One shared password and opaque in-memory sessions. Restarting the example
/// invalidates every session; it has no database or persistent auth state.
#[derive(Clone)]
pub struct Auth {
    password: Arc<str>,
    sessions: Arc<Mutex<HashMap<String, Instant>>>,
    cookie_secure: bool,
}

impl Auth {
    pub fn new(password: String, cookie_secure: bool) -> Self {
        Self {
            password: password.into(),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            cookie_secure,
        }
    }

    pub fn login(&self, candidate: &str) -> Option<Login> {
        let expected = self.password.as_bytes();
        let actual = candidate.as_bytes();
        // Length is not secret; for equal-length inputs compare all bytes in
        // constant time so a password prefix is not a timing oracle.
        if expected.len() != actual.len() || expected.ct_eq(actual).unwrap_u8() != 1 {
            return None;
        }

        let mut random = [0_u8; 32];
        fill(&mut random).ok()?;
        let token = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        self.sessions.lock().ok()?.insert(
            token.clone(),
            Instant::now() + Duration::from_secs(SESSION_SECONDS as u64),
        );
        Some(Login {
            cookie: session_cookie(&token, false, self.cookie_secure),
        })
    }

    pub fn is_authenticated(&self, headers: &axum::http::HeaderMap) -> bool {
        let Some(token) = session_token(headers) else {
            return false;
        };
        let Ok(mut sessions) = self.sessions.lock() else {
            return false;
        };
        let now = Instant::now();
        sessions.retain(|_, expires| *expires > now);
        sessions.get(&token).is_some_and(|expires| *expires > now)
    }

    pub fn logout(&self, headers: &axum::http::HeaderMap) {
        if let Some(token) = session_token(headers) {
            if let Ok(mut sessions) = self.sessions.lock() {
                sessions.remove(&token);
            }
        }
    }

    pub fn clear_cookie(&self) -> HeaderValue {
        session_cookie("", true, self.cookie_secure)
    }
}

pub struct Login {
    pub cookie: HeaderValue,
}

fn session_cookie(token: &str, clear: bool, secure: bool) -> HeaderValue {
    let secure = if secure { "; Secure" } else { "" };
    let value = if clear {
        format!("{COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{secure}")
    } else {
        format!(
            "{COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={SESSION_SECONDS}{secure}"
        )
    };
    HeaderValue::from_str(&value).expect("cookie contains only safe hex characters")
}

fn session_token(headers: &axum::http::HeaderMap) -> Option<String> {
    let cookie = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    cookie.split(';').find_map(|piece| {
        let value = piece.trim().strip_prefix(&format!("{COOKIE}="))?;
        (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .then(|| value.to_owned())
    })
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, header::COOKIE};

    use super::Auth;

    #[test]
    fn login_mints_a_cookie_and_logout_invalidates_it() {
        let auth = Auth::new("correct horse battery staple".to_owned(), true);
        assert!(auth.login("wrong").is_none());
        let login = auth.login("correct horse battery staple").unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(COOKIE, login.cookie);
        assert!(auth.is_authenticated(&headers));
        auth.logout(&headers);
        assert!(!auth.is_authenticated(&headers));
    }

    #[test]
    fn a_malformed_cookie_is_not_a_session() {
        let auth = Auth::new("secret".to_owned(), false);
        let mut headers = HeaderMap::new();
        headers.insert(COOKIE, "atlas_admin=not-hex".parse().unwrap());
        assert!(!auth.is_authenticated(&headers));
    }
}
