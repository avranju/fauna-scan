//! Login handlers and the authentication boundary for pages, APIs, and media.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Extension, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Semaphore;

use super::{WebError, WebState};
use crate::authentication::{
    fingerprint, hash_password, new_session_token, validate_credentials, verify_password,
};

const COOKIE: &str = "fauna_scan_session";
const COOKIE_LIFETIME: i64 = 400 * 24 * 60 * 60;
const WINDOW: Duration = Duration::from_secs(60);

/// Bound both online guesses and the CPU/memory cost of Argon2 verification.
#[derive(Clone)]
pub(super) struct LoginGuard {
    attempts: Arc<Mutex<HashMap<String, (Instant, u32)>>>,
    hashing: Arc<Semaphore>,
}

impl Default for LoginGuard {
    fn default() -> Self {
        Self {
            attempts: Default::default(),
            hashing: Arc::new(Semaphore::new(2)),
        }
    }
}

impl LoginGuard {
    fn admit(&self, username: &str) -> bool {
        let mut attempts = self.attempts.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        attempts.retain(|_, (start, _)| now.duration_since(*start) < WINDOW);
        // The global key is distinct from the hashed account keys. The global
        // budget also bounds the number of entries for nonexistent accounts.
        let global = attempts.entry(String::new()).or_insert((now, 0));
        if global.1 >= 60 {
            return false;
        }
        global.1 += 1;
        let account = attempts.entry(fingerprint(username)).or_insert((now, 0));
        if account.1 >= 5 {
            return false;
        }
        account.1 += 1;
        true
    }
}

#[derive(Clone)]
pub(super) struct LoggedIn(pub String);

fn error(status: StatusCode, code: &'static str, message: &str) -> WebError {
    WebError {
        status,
        code,
        message: message.into(),
    }
}

fn unauthorized() -> WebError {
    error(
        StatusCode::UNAUTHORIZED,
        "unauthenticated",
        "Please sign in to continue.",
    )
}

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    let mut tokens = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|part| part.trim().split_once('='))
        .filter(|(name, _)| *name == COOKIE)
        .map(|(_, token)| token);
    let token = tokens.next()?;
    // Reject duplicate cookies and malformed tokens before database access.
    if tokens.next().is_some()
        || token.len() != 43
        || !token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    Some(token.to_owned())
}

fn set_cookie(response: &mut Response, state: &WebState, token: &str, lifetime: i64) {
    let secure = if state.web.secure_cookie {
        "; Secure"
    } else {
        ""
    };
    let value =
        format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={lifetime}{secure}");
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&value).expect("safe session cookie"),
    );
}

/// A custom header on unsafe requests prevents cross-site form and fetch
/// attacks. There is deliberately no CORS allowance for this header.
pub(super) async fn require_login(
    State(state): State<WebState>,
    mut request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    let public = path == "/login" || path.starts_with("/assets/") || path == "/api/v1/auth/login";
    if !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) && request
        .headers()
        .get("x-fauna-scan-request")
        .and_then(|value| value.to_str().ok())
        != Some("1")
    {
        return error(
            StatusCode::FORBIDDEN,
            "invalid_origin",
            "A same-origin request is required.",
        )
        .into_response();
    }
    if public {
        return next.run(request).await;
    }
    let session = match cookie_token(request.headers()) {
        Some(token) => match state
            .ops
            .find_session(&fingerprint(&token), Utc::now().timestamp())
            .await
        {
            Ok(Some(session))
                if session.credential_fingerprint == fingerprint(&session.password_hash) =>
            {
                Some((token, session))
            }
            Ok(_) => None,
            Err(error) => return WebError::from(error).into_response(),
        },
        None => None,
    };
    let Some((token, session)) = session else {
        let mut response = if path.starts_with("/api/") {
            unauthorized().into_response()
        } else {
            Redirect::to("/login").into_response()
        };
        set_cookie(&mut response, &state, "", 0);
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, no-store"),
        );
        return response;
    };
    let lifetime = session
        .expires_at
        .map(|expiry| {
            expiry
                .saturating_sub(Utc::now().timestamp())
                .clamp(0, COOKIE_LIFETIME)
        })
        .unwrap_or(COOKIE_LIFETIME);
    let logging_out = path == "/api/v1/auth/logout";
    request.extensions_mut().insert(LoggedIn(session.username));
    let mut response = next.run(request).await;
    if !logging_out {
        set_cookie(&mut response, &state, &token, lifetime);
    }
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Login {
    username: String,
    password: String,
}

pub(super) async fn login(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(login): Json<Login>,
) -> Result<Response, WebError> {
    let invalid = || {
        error(
            StatusCode::UNAUTHORIZED,
            "invalid_credentials",
            "Invalid user name or password.",
        )
    };
    if validate_credentials(&login.username, &login.password).is_err() {
        return Err(invalid());
    }
    if !state.login_guard.admit(&login.username) {
        return Err(error(
            StatusCode::TOO_MANY_REQUESTS,
            "login_throttled",
            "Too many sign-in attempts. Try again in a minute.",
        ));
    }
    let permit = state
        .login_guard
        .hashing
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            error(
                StatusCode::TOO_MANY_REQUESTS,
                "login_busy",
                "Sign-in is busy. Please try again shortly.",
            )
        })?;
    let user = state.ops.find_user(&login.username).await?;
    let verified = tokio::task::spawn_blocking(move || {
        // Keep the permit until hashing actually finishes, even if the HTTP
        // future times out. Unknown accounts do equivalent password work.
        let _permit = permit;
        static DUMMY: OnceLock<String> = OnceLock::new();
        let dummy =
            DUMMY.get_or_init(|| hash_password(&new_session_token()).expect("dummy password hash"));
        let encoded = user
            .as_ref()
            .map(|user| user.password_hash.as_str())
            .unwrap_or(dummy);
        let valid = verify_password(&login.password, encoded);
        (valid, user)
    })
    .await
    .map_err(|_| {
        error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "login_failed",
            "Sign-in could not be completed.",
        )
    })?;
    let (true, Some(user)) = verified else {
        return Err(invalid());
    };
    let expires_at = if state.web.session_expiry_seconds == 0 {
        None
    } else {
        Some(
            Utc::now()
                .timestamp()
                .checked_add(state.web.session_expiry_seconds as i64)
                .ok_or_else(|| WebError::bad_request("Session lifetime is too large"))?,
        )
    };
    let token = new_session_token();
    if !state
        .ops
        .create_session(&fingerprint(&token), &user, expires_at)
        .await?
    {
        return Err(invalid());
    }
    // Replacing a cookie also revokes its previous server-side session.
    if let Some(previous) = cookie_token(&headers) {
        state.ops.remove_session(&fingerprint(&previous)).await?;
    }
    let mut response = Json(json!({ "username": user.username })).into_response();
    let lifetime = expires_at
        .map(|expiry| {
            expiry
                .saturating_sub(Utc::now().timestamp())
                .clamp(0, COOKIE_LIFETIME)
        })
        .unwrap_or(COOKIE_LIFETIME);
    set_cookie(&mut response, &state, &token, lifetime);
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    Ok(response)
}

pub(super) async fn session(Extension(user): Extension<LoggedIn>) -> Json<serde_json::Value> {
    Json(json!({ "username": user.0 }))
}

pub(super) async fn logout(
    State(state): State<WebState>,
    headers: HeaderMap,
) -> Result<Response, WebError> {
    if let Some(token) = cookie_token(&headers) {
        state.ops.remove_session(&fingerprint(&token)).await?;
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    set_cookie(&mut response, &state, "", 0);
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use tower::ServiceExt;

    async fn request(
        state: &WebState,
        method: &str,
        path: &str,
        cookie: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> Response {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header("x-fauna-scan-request", "1");
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        let body = if let Some(body) = body {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(body.to_string())
        } else {
            Body::empty()
        };
        super::super::router(state.clone())
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap()
    }

    async fn credentials(state: &WebState) {
        let hash = hash_password("test password").unwrap();
        assert!(state.ops.add_user("Test User", &hash).await.unwrap());
    }

    async fn sign_in(state: &WebState) -> String {
        let response = request(
            state,
            "POST",
            "/api/v1/auth/login",
            None,
            Some(json!({"username": "Test User", "password": "test password"})),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let value = response.headers()[header::SET_COOKIE].to_str().unwrap();
        assert!(value.contains("HttpOnly"));
        assert!(value.contains("SameSite=Lax"));
        assert!(value.contains("Path=/"));
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "private, no-store"
        );
        value.split(';').next().unwrap().to_string()
    }

    #[tokio::test]
    async fn anonymous_requests_cannot_reach_pages_api_images_clips_or_events() {
        let (_root, state) = super::super::tests::test_state().await;
        for path in [
            "/",
            "/images",
            "/images/1",
            "/activity",
            "/about",
            "/index.html",
        ] {
            let response = request(&state, "GET", path, None, None).await;
            assert_eq!(response.status(), StatusCode::SEE_OTHER, "{path}");
            assert_eq!(response.headers()[header::LOCATION], "/login");
            assert_eq!(
                response.headers()[header::CACHE_CONTROL],
                "private, no-store"
            );
        }
        for path in [
            "/api/v1/config",
            "/api/v1/health",
            "/api/v1/cameras",
            "/api/v1/overview",
            "/api/v1/images",
            "/api/v1/images/facets",
            "/api/v1/images/1",
            "/api/v1/images/1/neighbors",
            "/api/v1/images/1/content",
            "/api/v1/images/1/thumbnail",
            "/api/v1/images/1/recording",
            "/api/v1/clips/token",
            "/api/v1/activity",
            "/api/v1/events",
            "/api/v1/auth/session",
        ] {
            assert_eq!(
                request(&state, "GET", path, None, None).await.status(),
                StatusCode::UNAUTHORIZED,
                "{path}"
            );
        }
        assert_eq!(
            request(&state, "POST", "/api/v1/images/1/clip", None, None)
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        if super::super::assets::available() {
            assert_eq!(
                request(&state, "GET", "/login", None, None).await.status(),
                StatusCode::OK
            );
        }
        assert_eq!(
            request(
                &state,
                "GET",
                "/api/v1/auth/session",
                Some("fauna_scan_session=invalid"),
                None
            )
            .await
            .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(request(&state, "GET", "/api/v1/auth/session", Some("fauna_scan_session=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA; fauna_scan_session=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"), None).await.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn invalid_passwords_unknown_users_and_plaintext_rows_get_generic_errors() {
        let (_root, state) = super::super::tests::test_state().await;
        credentials(&state).await;
        state
            .ops
            .add_user("plaintext", "test password")
            .await
            .unwrap();
        for (username, password) in [
            ("Test User", "wrong"),
            ("unknown", "test password"),
            ("plaintext", "test password"),
        ] {
            let response = request(
                &state,
                "POST",
                "/api/v1/auth/login",
                None,
                Some(json!({"username": username, "password": password})),
            )
            .await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert!(response.headers().get(header::SET_COOKIE).is_none());
            let body: serde_json::Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap())
                    .unwrap();
            assert_eq!(body["error"]["message"], "Invalid user name or password.");
        }
    }

    #[tokio::test]
    async fn sessions_survive_database_reconnect_and_logout_revokes_them() {
        let (root, state) = super::super::tests::test_state().await;
        credentials(&state).await;
        let cookie = sign_in(&state).await;
        let token = cookie.split_once('=').unwrap().1;
        let store =
            crate::database::sqlite::SqliteDataStore::connect(&root.path().join("web.sqlite3"), 1)
                .await
                .unwrap();
        let stored: (String, Option<i64>) = sqlx::query_as(
            "SELECT token_hash, expires_at FROM web_sessions WHERE username = 'Test User'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(stored.0, fingerprint(token));
        assert_ne!(stored.0, token);
        assert_eq!(stored.1, None);
        let mut restarted = state.clone();
        restarted.ops = store.ops();
        restarted.login_guard = Default::default();
        for path in [
            "/api/v1/auth/session",
            "/api/v1/config",
            "/api/v1/images",
            "/api/v1/images/1/content",
        ] {
            let response = request(&restarted, "GET", path, Some(&cookie), None).await;
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert!(
                response.headers()[header::SET_COOKIE]
                    .to_str()
                    .unwrap()
                    .contains("Max-Age=34560000")
            );
        }
        if super::super::assets::available() {
            for path in ["/", "/images", "/images/1", "/activity", "/about"] {
                assert_eq!(
                    request(&restarted, "GET", path, Some(&cookie), None)
                        .await
                        .status(),
                    StatusCode::OK,
                    "{path}"
                );
            }
        }
        let response = request(
            &restarted,
            "POST",
            "/api/v1/auth/logout",
            Some(&cookie),
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            response
                .headers()
                .get_all(header::SET_COOKIE)
                .iter()
                .count(),
            1
        );
        assert!(
            response.headers()[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .contains("Max-Age=0")
        );
        assert_eq!(
            request(&state, "GET", "/api/v1/auth/session", Some(&cookie), None)
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn removing_and_recreating_a_user_does_not_restore_old_sessions() {
        let (_root, state) = super::super::tests::test_state().await;
        credentials(&state).await;
        let cookie = sign_in(&state).await;
        assert!(state.ops.remove_user("Test User").await.unwrap());
        credentials(&state).await;
        assert_eq!(
            request(&state, "GET", "/api/v1/auth/session", Some(&cookie), None)
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn password_changes_and_configured_expiry_invalidate_sessions() {
        let (root, mut state) = super::super::tests::test_state().await;
        credentials(&state).await;
        state.web.session_expiry_seconds = 3600;
        state.web.secure_cookie = true;
        let cookie = sign_in(&state).await;
        let response = request(&state, "GET", "/api/v1/auth/session", Some(&cookie), None).await;
        let header = response.headers()[header::SET_COOKIE].to_str().unwrap();
        assert!(header.contains("Secure"));
        let age: i64 = header
            .split("Max-Age=")
            .nth(1)
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!((3598..=3600).contains(&age));
        let store =
            crate::database::sqlite::SqliteDataStore::connect(&root.path().join("web.sqlite3"), 1)
                .await
                .unwrap();
        sqlx::query("UPDATE web_sessions SET expires_at = ? WHERE username = 'Test User'")
            .bind(Utc::now().timestamp())
            .execute(store.pool())
            .await
            .unwrap();
        assert_eq!(
            request(&state, "GET", "/api/v1/auth/session", Some(&cookie), None)
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let cookie = sign_in(&state).await;
        sqlx::query("UPDATE users SET password_hash = ? WHERE username = 'Test User'")
            .bind(hash_password("new password").unwrap())
            .execute(store.pool())
            .await
            .unwrap();
        assert_eq!(
            request(&state, "GET", "/api/v1/auth/session", Some(&cookie), None)
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn unsafe_requests_require_the_csrf_header_and_login_work_is_limited() {
        let (_root, state) = super::super::tests::test_state().await;
        for path in [
            "/api/v1/auth/login",
            "/api/v1/auth/logout",
            "/api/v1/images/1/clip",
        ] {
            let response = super::super::router(state.clone())
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
        for _ in 0..5 {
            assert!(state.login_guard.admit("user"));
        }
        assert!(!state.login_guard.admit("user"));
        for index in 0..54 {
            assert!(state.login_guard.admit(&format!("user-{index}")));
        }
        assert!(!state.login_guard.admit("another user"));
        let (_root, state) = super::super::tests::test_state().await;
        let _permit = state
            .login_guard
            .hashing
            .clone()
            .acquire_many_owned(2)
            .await
            .unwrap();
        assert_eq!(
            request(
                &state,
                "POST",
                "/api/v1/auth/login",
                None,
                Some(json!({"username": "user", "password": "password"}))
            )
            .await
            .status(),
            StatusCode::TOO_MANY_REQUESTS
        );
    }
}
