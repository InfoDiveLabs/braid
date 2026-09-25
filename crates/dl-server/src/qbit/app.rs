//! The identity and session half of the qBittorrent-compatible API: enough
//! for Sonarr, Radarr and Prowlarr to log in and ask who they are talking to.
//!
//! These clients do not speak a generic download-client protocol. They speak
//! qBittorrent's own Web API, so this server answers as qBittorrent would:
//! same login handshake, same version strings, same body text on success and
//! failure. See [`QBITTORRENT_VERSION`] for why that is a deliberate claim
//! rather than an accident.
//!
//! Every route here answers without the shared `require_auth` middleware
//! layered on it, and that split is not an oversight. `auth/login` obviously
//! cannot require a session it exists to grant, and every other route here
//! has to answer a request with no session at all with `403`, because that is
//! what a qBittorrent client is written to expect from *its* unauthenticated
//! response, not the `401` this server's own API would send. Rather than
//! teach the shared middleware two different failure codes for two different
//! callers, each handler below checks the session itself.

use crate::auth::{Credentials, Sessions, request_is_secure, session_cookie};
use crate::state::AppState;
use axum::extract::{Extension, Form, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use std::sync::Arc;

/// The version this server claims to be, and the reason there is a claim at
/// all: Sonarr, Radarr and Prowlarr do not talk to a generic download
/// client, they talk to qBittorrent's own Web API
/// (<https://github.com/qbittorrent/qBittorrent/wiki/WebUI-API-(qBittorrent-4.1)>),
/// and refuse to add a client reporting a version below what that API
/// guarantees. Claiming to be qBittorrent is the entire point of this
/// module, not a detail buried inside it, so it is named and explained here
/// rather than left for a reader to work out from a string literal.
const QBITTORRENT_VERSION: &str = "v4.6.0";

/// The Web API version that goes with [`QBITTORRENT_VERSION`]. Reported
/// separately because that is what these clients check separately: some
/// gate on the application version, some on this one.
const WEBAPI_VERSION: &str = "2.9.2";

/// The cookie name qBittorrent's own clients send, matching
/// `auth::SESSION_COOKIE`. Not imported from there: that constant is private
/// to a module that predates this one and has no other reason to expose it,
/// and the name is part of the wire protocol these clients speak, not an
/// implementation detail that module happens to own.
const SESSION_COOKIE: &str = "SID";

/// Pull the session id out of a raw `Cookie` header, the same way
/// `auth::require_auth` does for the routes that sit behind it. Duplicated
/// rather than shared because that function is private to `auth`, which owns
/// nothing else these routes need: reaching into it for one line of parsing
/// would trade a few lines here for a dependency on that module's internals.
fn session_id(headers: &HeaderMap) -> Option<&str> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|pair| {
        let (key, value) = pair.trim().split_once('=')?;
        (key == SESSION_COOKIE).then_some(value)
    })
}

/// Whether this request carries a session the shared store still recognises.
fn authenticated(headers: &HeaderMap, sessions: &Sessions) -> bool {
    session_id(headers).is_some_and(|id| sessions.valid(id))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v2/auth/login", post(login))
        .route("/api/v2/auth/logout", post(logout))
        .route("/api/v2/app/version", get(version))
        .route("/api/v2/app/webapiVersion", get(webapi_version))
        .route("/api/v2/app/preferences", get(preferences))
        .route("/api/v2/app/setPreferences", post(set_preferences))
        .route("/api/v2/transfer/info", get(transfer_info))
}

#[derive(serde::Deserialize)]
struct LoginForm {
    username: String,
    password: String,
}

/// `POST /api/v2/auth/login`.
///
/// Issues into the same [`Sessions`] store `auth::require_auth` checks, so a
/// client that logs in through this endpoint is not mysteriously
/// unauthenticated against the rest of the server, and vice versa: one store,
/// shared, rather than two that would have to be kept in agreement forever.
///
/// The body on success is the literal string `Ok.`, full stop included, and
/// on failure `Fails.` with the same `200` status. Real qBittorrent clients
/// compare that body rather than the status code: a wrong password is not a
/// transport-level failure to them, so answering `401` or `403` here would
/// read to them as a broken server rather than as bad credentials.
async fn login(
    Extension(sessions): Extension<Arc<Sessions>>,
    Extension(credentials): Extension<Arc<Credentials>>,
    headers: HeaderMap,
    Form(body): Form<LoginForm>,
) -> Response {
    if !credentials.verify(&body.username, &body.password) {
        return (StatusCode::OK, "Fails.").into_response();
    }

    let id = sessions.issue();
    let cookie = session_cookie(&id, request_is_secure(&headers));
    match header::HeaderValue::from_str(&cookie) {
        Ok(value) => ([(header::SET_COOKIE, value)], "Ok.").into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// `POST /api/v2/auth/logout`. Revoked immediately rather than left to
/// expire, for the same reason `auth::logout` revokes rather than expires:
/// somebody logging out means it now.
async fn logout(Extension(sessions): Extension<Arc<Sessions>>, headers: HeaderMap) -> Response {
    if let Some(id) = session_id(&headers) {
        sessions.revoke(id);
    }
    (StatusCode::OK, "Ok.").into_response()
}

/// `GET /api/v2/app/version`.
async fn version(Extension(sessions): Extension<Arc<Sessions>>, headers: HeaderMap) -> Response {
    if !authenticated(&headers, &sessions) {
        return StatusCode::FORBIDDEN.into_response();
    }
    (StatusCode::OK, QBITTORRENT_VERSION).into_response()
}

/// `GET /api/v2/app/webapiVersion`.
async fn webapi_version(
    Extension(sessions): Extension<Arc<Sessions>>,
    headers: HeaderMap,
) -> Response {
    if !authenticated(&headers, &sessions) {
        return StatusCode::FORBIDDEN.into_response();
    }
    (StatusCode::OK, WEBAPI_VERSION).into_response()
}

/// The subset of qBittorrent's preferences these clients actually read.
///
/// Every field here is either a fact about this container (`save_path`) or
/// an honest "we do not do that" (the ratio and seeding-time settings, which
/// this server has no concept of at all). Omitted entirely rather than
/// invented: a client reading a fabricated value for something we do not
/// implement is worse than a client that never asked, because it now
/// believes a setting is in effect that has no effect on anything here.
#[derive(serde::Serialize)]
struct Preferences {
    /// What Sonarr reads to work out where a finished file will land. Wrong
    /// here breaks importing even when the download itself succeeded, since
    /// Sonarr goes looking for the file under this path rather than the one
    /// the transfer actually used.
    save_path: String,
    /// This server has no seed ratio target, so there is nothing to enable.
    max_ratio_enabled: bool,
    max_ratio: f64,
    max_seeding_time_enabled: bool,
    max_seeding_time: i64,
    /// qBittorrent's own convention: `0` means unlimited, not "unset".
    dl_limit: i64,
    up_limit: i64,
}

/// `GET /api/v2/app/preferences`.
async fn preferences(
    Extension(sessions): Extension<Arc<Sessions>>,
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Response {
    if !authenticated(&headers, &sessions) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let prefs = Preferences {
        save_path: state.config.download_dir.display().to_string(),
        max_ratio_enabled: false,
        max_ratio: -1.0,
        max_seeding_time_enabled: false,
        max_seeding_time: -1,
        dl_limit: state.config.download_limit.map_or(0, |v| v as i64),
        up_limit: state.config.upload_limit.map_or(0, |v| v as i64),
    };
    Json(prefs).into_response()
}

/// Whatever a client sends is accepted and ignored rather than parsed: the
/// `json` field carries a blob of settings only real qBittorrent understands,
/// and this server implements none of them.
#[derive(serde::Deserialize)]
struct SetPreferencesForm {
    #[serde(default)]
    #[allow(dead_code)]
    json: String,
}

/// `POST /api/v2/app/setPreferences`.
///
/// Sonarr, Radarr and Prowlarr write settings defensively at every startup,
/// not because anything changed. Answering anything but success here would
/// have them log an error forever over a write they never needed to make in
/// the first place, for settings that have no effect on how this server
/// behaves either way.
async fn set_preferences(
    Extension(sessions): Extension<Arc<Sessions>>,
    headers: HeaderMap,
    Form(_body): Form<SetPreferencesForm>,
) -> Response {
    if !authenticated(&headers, &sessions) {
        return StatusCode::FORBIDDEN.into_response();
    }
    StatusCode::OK.into_response()
}

#[derive(serde::Serialize)]
struct TransferInfo {
    dl_info_speed: u64,
    up_info_speed: u64,
}

/// `GET /api/v2/transfer/info`.
///
/// `dl_info_speed` is `Engine::total_bytes_per_sec`, the same smoothed
/// combined rate the web UI's own toolbar reads. `up_info_speed` has no
/// equivalent already sitting on `Engine`: upload only exists on the torrent
/// path, so it is summed here from every transfer's `TorrentStatus` rather
/// than left at a constant zero, which would tell Sonarr this server never
/// uploads anything even while a torrent is actively seeding.
async fn transfer_info(
    Extension(sessions): Extension<Arc<Sessions>>,
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Response {
    if !authenticated(&headers, &sessions) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let dl_info_speed = state.engine.total_bytes_per_sec();
    let up_info_speed: u64 = state
        .engine
        .snapshot()
        .iter()
        .filter_map(|snapshot| snapshot.torrent.as_ref())
        .map(|torrent| torrent.upload_bytes_per_sec)
        .sum();
    Json(TransferInfo { dl_info_speed, up_info_speed }).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth;
    use dl_core::budget::Budget;
    use dl_core::engine::{DownloadSpec, Engine, EngineConfig, SourceFactory};
    use dl_core::lane::LaneSet;
    use http_body_util::BodyExt;
    use std::path::PathBuf;
    use tower::ServiceExt;

    struct NoSources;

    impl SourceFactory for NoSources {
        fn lanes_for(&self, _spec: &DownloadSpec) -> dl_core::Result<Box<dyn LaneSet>> {
            unreachable!("nothing in this suite starts a transfer")
        }
    }

    fn test_state(download_dir: &str) -> AppState {
        let engine = Engine::new(
            Arc::new(NoSources),
            EngineConfig { max_concurrent: 0, ..Default::default() },
            Budget::unlimited(),
        );
        let config = crate::config::Config {
            download_dir: PathBuf::from(download_dir),
            ..Default::default()
        };
        AppState { engine, config: Arc::new(config) }
    }

    /// A fresh `Credentials` and the one password that verifies against it.
    fn test_credentials() -> (Credentials, String) {
        let dir = tempfile::tempdir().unwrap();
        let (credentials, password) = Credentials::load_or_create(dir.path()).unwrap();
        (credentials, password.expect("freshly created credentials hand back their password"))
    }

    fn app(state: AppState, sessions: Arc<Sessions>, credentials: Credentials) -> Router {
        routes()
            .with_state(state)
            .layer(Extension(sessions))
            .layer(Extension(Arc::new(credentials)))
    }

    fn form(uri: &str, body: &str) -> axum::http::Request<axum::body::Body> {
        axum::http::Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(axum::body::Body::from(body.to_string()))
            .unwrap()
    }

    fn get_request(uri: &str, session: Option<&str>) -> axum::http::Request<axum::body::Body> {
        let mut builder = axum::http::Request::builder().method("GET").uri(uri);
        if let Some(id) = session {
            builder = builder.header("cookie", format!("SID={id}"));
        }
        builder.body(axum::body::Body::empty()).unwrap()
    }

    async fn body_text(response: axum::http::Response<axum::body::Body>) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    async fn body_json(response: axum::http::Response<axum::body::Body>) -> serde_json::Value {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn sid_from(response: &axum::http::Response<axum::body::Body>) -> String {
        let raw = response.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap();
        let pair = raw.split(';').next().unwrap();
        pair.split_once('=').unwrap().1.to_string()
    }

    #[tokio::test]
    async fn login_answers_the_literal_string_clients_check_for() {
        // qBittorrent replies with the word `Ok.`, full stop included, and clients
        // compare the body rather than the status code.
        let (credentials, password) = test_credentials();
        let sessions = Arc::new(Sessions::default());
        let router = app(test_state("/downloads"), sessions, credentials);

        let response = router
            .oneshot(form("/api/v2/auth/login", &format!("username=admin&password={password}")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().contains_key(header::SET_COOKIE));
        assert_eq!(body_text(response).await, "Ok.");
    }

    #[tokio::test]
    async fn a_wrong_password_answers_fails_rather_than_an_error_status() {
        // qBittorrent answers 200 with a failure body, not 401 or 403. A client
        // that gets a status code it did not expect treats the server as broken
        // rather than the credentials as wrong.
        let (credentials, _password) = test_credentials();
        let sessions = Arc::new(Sessions::default());
        let router = app(test_state("/downloads"), sessions, credentials);

        let response = router
            .oneshot(form("/api/v2/auth/login", "username=admin&password=wrong"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_text(response).await, "Fails.");
    }

    #[tokio::test]
    async fn the_version_satisfies_what_the_arr_applications_require() {
        // These clients refuse to talk to anything below a minimum, so the
        // reported version is load bearing rather than decorative.
        let (credentials, _password) = test_credentials();
        let sessions = Arc::new(Sessions::default());
        let id = sessions.issue();
        let router = app(test_state("/downloads"), sessions, credentials);

        let response =
            router.clone().oneshot(get_request("/api/v2/app/version", Some(&id))).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let version = body_text(response).await;
        let numeric = version.trim_start_matches('v');
        let parts: Vec<u32> = numeric.split('.').map(|p| p.parse().unwrap()).collect();
        assert!(
            (parts[0], parts[1]) >= (4, 1),
            "Sonarr and Radarr refuse anything below qBittorrent 4.1, got {version}"
        );

        let response =
            router.oneshot(get_request("/api/v2/app/webapiVersion", Some(&id))).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let webapi = body_text(response).await;
        let parts: Vec<u32> = webapi.split('.').map(|p| p.parse().unwrap()).collect();
        assert!((parts[0], parts[1]) >= (2, 0), "got {webapi}");
    }

    #[tokio::test]
    async fn preferences_report_the_save_path_the_container_actually_uses() {
        // Sonarr reads `save_path` to work out where files will appear, and a
        // wrong answer breaks importing even when the download succeeded.
        let (credentials, _password) = test_credentials();
        let sessions = Arc::new(Sessions::default());
        let id = sessions.issue();
        let router = app(test_state("/downloads/complete"), sessions, credentials);

        let response =
            router.oneshot(get_request("/api/v2/app/preferences", Some(&id))).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["save_path"], "/downloads/complete");
    }

    #[tokio::test]
    async fn a_session_from_the_compatible_login_works_on_braids_own_api() {
        // One session store, or a client that logged in one way is mysteriously
        // unauthenticated the other.
        let (credentials, password) = test_credentials();
        let sessions = Arc::new(Sessions::default());

        let protected = Router::new()
            .route("/api/v1/probe", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn(auth::require_auth));

        let combined = Router::new()
            .merge(routes())
            .merge(protected)
            .with_state(test_state("/downloads"))
            .layer(Extension(sessions))
            .layer(Extension(Arc::new(credentials)));

        let login_response = combined
            .clone()
            .oneshot(form("/api/v2/auth/login", &format!("username=admin&password={password}")))
            .await
            .unwrap();
        assert_eq!(login_response.status(), StatusCode::OK);
        let id = sid_from(&login_response);

        let response =
            combined.clone().oneshot(get_request("/api/v1/probe", Some(&id))).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "a session from the qBittorrent-compatible login must satisfy Braid's own middleware"
        );

        let refused = combined.oneshot(get_request("/api/v1/probe", None)).await.unwrap();
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn set_preferences_accepts_what_it_does_not_implement() {
        // Clients write settings defensively at every startup. Refusing makes
        // them log an error forever over something they did not need.
        let (credentials, _password) = test_credentials();
        let sessions = Arc::new(Sessions::default());
        let id = sessions.issue();
        let router = app(test_state("/downloads"), sessions, credentials);

        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/api/v2/app/setPreferences")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("cookie", format!("SID={id}"))
            .body(axum::body::Body::from("json=%7B%22max_ratio_enabled%22%3Atrue%7D".to_string()))
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn an_unauthenticated_request_is_refused_with_403_not_401() {
        // qBittorrent's own clients expect 403 for an unauthenticated request
        // against any route other than login, matching what auth::require_auth
        // already answers for Braid's own API.
        let (credentials, _password) = test_credentials();
        let sessions = Arc::new(Sessions::default());
        let router = app(test_state("/downloads"), sessions, credentials);

        let response = router.oneshot(get_request("/api/v2/app/version", None)).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
}
