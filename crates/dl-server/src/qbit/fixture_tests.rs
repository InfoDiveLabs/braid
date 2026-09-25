//! Replays real Sonarr and Radarr traffic, recorded through the logging
//! proxy in `harness/` against a real Sonarr, a real Radarr and this very
//! server, and asserts our handlers reproduce what was actually observed.
//!
//! Every fixture under `tests/fixtures/` is a single exchange from that
//! recording, not a guess at one: see `harness/README.md` for how each was
//! produced and what it is evidence of. A field that depends on wall clock
//! time or on a live network measurement (`added_on`, `dlspeed`, `eta` while
//! a transfer is still moving) cannot be reproduced byte for byte here, and
//! each test below says exactly which fields it is and is not asserting on,
//! rather than pretending a broader match than the fixture can actually
//! support.

use crate::auth::{self, Credentials, Sessions};
use crate::qbit::{app, torrents};
use crate::state::AppState;
use axum::Extension;
use axum::Router;
use axum::http::{StatusCode, header};
use dl_core::budget::Budget;
use dl_core::engine::{DownloadSpec, Engine, EngineConfig, SourceFactory};
use dl_core::error::{Error, Result as EngineResult};
use dl_core::lane::LaneSet;
use dl_core::torrent::{
    TorrentBackend, TorrentOutcome, TorrentProgress, TorrentRequest, TorrentSource, TorrentStatus,
};
use dl_core::{Progress, State};
use http_body_util::BodyExt;
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

/// One recorded exchange. Mirrors the JSON shape under `tests/fixtures/`
/// exactly, so a fixture can be read here without a hand translation that
/// could quietly drift from what is actually on disk.
#[derive(Deserialize)]
struct Fixture {
    #[allow(dead_code)]
    recorded_from: String,
    #[allow(dead_code)]
    note: Option<String>,
    request: FixtureRequest,
    response: FixtureResponse,
}

#[derive(Deserialize)]
struct FixtureRequest {
    method: String,
    path: String,
    content_type: Option<String>,
    body: Option<String>,
    /// Documentary only: every test below chooses whether to attach a
    /// session itself, since what a fixture is evidence of and what a given
    /// test wants to exercise are not always the same request.
    #[serde(default)]
    #[allow(dead_code)]
    authenticated: bool,
}

#[derive(Deserialize)]
struct FixtureResponse {
    status: u16,
    /// Documentary only: not every handler here sets this header, and the
    /// tests below check the body, which is the part a real client parses.
    #[allow(dead_code)]
    content_type: Option<String>,
    body: Option<String>,
    #[serde(default)]
    sets_session_cookie: bool,
}

/// Loads one fixture by name, embedding the JSON at compile time so this
/// suite has no runtime dependency on where the crate happens to be checked
/// out.
macro_rules! fixture {
    ($name:literal) => {
        serde_json::from_str::<Fixture>(include_str!(concat!(
            "../../tests/fixtures/",
            $name,
            ".json"
        )))
        .expect(concat!($name, ".json does not parse as a fixture"))
    };
}

struct NoFactory;

impl SourceFactory for NoFactory {
    fn lanes_for(&self, _spec: &DownloadSpec) -> EngineResult<Box<dyn LaneSet>> {
        Err(Error::NoRouteAvailable)
    }
}

/// A torrent backend that reports whatever progress a test hands it, once,
/// and then holds the transfer open rather than returning: returning early
/// runs the engine's real finish path, which drops everything but
/// `uploaded` and `files` from the torrent block (see
/// `dl_core::engine::Engine::run_torrent`) and would erase the very fields
/// these tests assert on.
struct FakeTorrentBackend {
    progress: TorrentProgress,
}

impl FakeTorrentBackend {
    fn new(progress: TorrentProgress) -> Arc<Self> {
        Arc::new(Self { progress })
    }
}

#[async_trait::async_trait]
impl TorrentBackend for FakeTorrentBackend {
    async fn run(&self, request: TorrentRequest) -> EngineResult<TorrentOutcome> {
        if let Some(on_progress) = &request.on_progress {
            on_progress(self.progress.clone());
        }
        while !request.cancel.is_cancelled() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Err(Error::Cancelled)
    }

    async fn discard(&self, _source: &TorrentSource, _delete_files: bool) -> EngineResult<()> {
        Ok(())
    }
}

/// The two routers Sonarr and Radarr actually talk to, wired the same way
/// `main.rs` wires them: `qbit::app`'s routes answer their own 403s and sit
/// outside the session middleware because `auth/login` lives there, and
/// `qbit::torrents`'s routes sit behind it exactly as they do in production.
/// Braid's own `/api/v1` and the UI are left out: neither client ever calls
/// either.
struct TestApp {
    _dir: tempfile::TempDir,
    state: AppState,
    sessions: Arc<Sessions>,
    credentials: Credentials,
    /// The real password `Credentials::load_or_create` generated for this
    /// instance, the same way every test in this crate that needs to log in
    /// does: there is no way to ask for a chosen one, and there should not
    /// be.
    password: String,
}

impl TestApp {
    fn with_backend(backend: Arc<FakeTorrentBackend>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        let download_dir = PathBuf::from("/downloads");
        std::fs::create_dir_all(&config_dir).unwrap();

        let engine = Engine::new(Arc::new(NoFactory), EngineConfig::default(), Budget::unlimited());
        engine.set_torrent_backend(backend);

        let config = crate::config::Config {
            config_dir: config_dir.clone(),
            download_dir,
            ..Default::default()
        };
        let state = AppState { engine, config: Arc::new(config) };

        let sessions = Arc::new(Sessions::default());
        let (credentials, generated) = Credentials::load_or_create(&config_dir).unwrap();
        let password = generated.expect("freshly created credentials hand back their password");

        Self { _dir: dir, state, sessions, credentials, password }
    }

    fn no_torrents() -> Self {
        Self::with_backend(FakeTorrentBackend::new(TorrentProgress {
            progress: Progress::default(),
            status: TorrentStatus::default(),
            name: None,
            seeding: false,
            phase: None,
        }))
    }

    fn router(&self) -> Router {
        let guarded = torrents::routes().layer(axum::middleware::from_fn(auth::require_auth));
        Router::new()
            .merge(app::routes())
            .merge(guarded)
            .with_state(self.state.clone())
            .layer(Extension(self.sessions.clone()))
            .layer(Extension(Arc::new(self.credentials.clone())))
    }

    /// Logs in the way every one of these fixtures assumes has already
    /// happened by the time the request they carry is sent, and hands back
    /// the `SID` to attach as a cookie.
    async fn login(&self) -> String {
        let body = format!("username=admin&password={}", self.password);
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/api/v2/auth/login")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(axum::body::Body::from(body))
            .unwrap();
        let response = self.router().oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "the login itself is not what this test is about"
        );
        let raw = response.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap();
        raw.split(';').next().unwrap().split_once('=').unwrap().1.to_string()
    }

    /// `torrents/createCategory` with the exact body Sonarr sent, so a
    /// category exists the same way it did against the real client: with no
    /// save path at all. See `sonarr_creates_a_category_with_no_save_path`.
    async fn create_tv_sonarr_category(&self, session: &str) {
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/api/v2/torrents/createCategory")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("cookie", format!("SID={session}"))
            .body(axum::body::Body::from("category=tv-sonarr"))
            .unwrap();
        let response = self.router().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}

async fn send(
    router: &Router,
    fixture: &FixtureRequest,
    session: Option<&str>,
) -> axum::http::Response<axum::body::Body> {
    let mut builder =
        axum::http::Request::builder().method(fixture.method.as_str()).uri(&fixture.path);
    if let Some(content_type) = &fixture.content_type {
        builder = builder.header("content-type", content_type);
    }
    if let Some(session) = session {
        builder = builder.header("cookie", format!("SID={session}"));
    }
    let body = axum::body::Body::from(fixture.body.clone().unwrap_or_default());
    router.clone().oneshot(builder.body(body).unwrap()).await.unwrap()
}

async fn body_text(response: axum::http::Response<axum::body::Body>) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn version_probe_before_login_is_refused() {
    let fixture = fixture!("version_probe_before_login_is_refused");
    let app = TestApp::no_torrents();

    let response = send(&app.router(), &fixture.request, None).await;

    assert_eq!(response.status().as_u16(), fixture.response.status);
    assert_eq!(body_text(response).await, fixture.response.body.unwrap_or_default());
}

#[tokio::test]
async fn login_succeeds_with_the_generated_password() {
    let fixture = fixture!("login_succeeds_with_the_generated_password");
    let app = TestApp::no_torrents();
    // The fixture carries the literal password Braid printed on the run this
    // was recorded from, replaced with a placeholder because that password
    // is meaningless outside that one run. This test's own instance
    // generated a different one, and substituting it in is what makes this
    // still the same request Sonarr actually sent.
    let body = fixture.request.body.as_deref().unwrap().replace("<PASSWORD>", &app.password);
    let mut request = fixture.request;
    request.body = Some(body);

    let response = send(&app.router(), &request, None).await;

    assert_eq!(response.status().as_u16(), fixture.response.status);
    assert_eq!(
        fixture.response.sets_session_cookie,
        response.headers().contains_key(header::SET_COOKIE)
    );
    assert_eq!(body_text(response).await, fixture.response.body.unwrap());
}

#[tokio::test]
async fn webapi_version_after_login() {
    let fixture = fixture!("webapi_version_after_login");
    let app = TestApp::no_torrents();
    let session = app.login().await;

    let response = send(&app.router(), &fixture.request, Some(&session)).await;

    assert_eq!(response.status().as_u16(), fixture.response.status);
    assert_eq!(body_text(response).await, fixture.response.body.unwrap());
}

#[tokio::test]
async fn preferences_reports_the_download_directory() {
    let fixture = fixture!("preferences_reports_the_download_directory");
    let app = TestApp::no_torrents();
    let session = app.login().await;

    let response = send(&app.router(), &fixture.request, Some(&session)).await;

    assert_eq!(response.status().as_u16(), fixture.response.status);
    let actual: Value = serde_json::from_str(&body_text(response).await).unwrap();
    let expected: Value = serde_json::from_str(&fixture.response.body.unwrap()).unwrap();
    assert_eq!(actual, expected);
}

#[tokio::test]
async fn categories_start_empty() {
    let fixture = fixture!("categories_start_empty");
    let app = TestApp::no_torrents();
    let session = app.login().await;

    let response = send(&app.router(), &fixture.request, Some(&session)).await;

    assert_eq!(response.status().as_u16(), fixture.response.status);
    assert_eq!(body_text(response).await, fixture.response.body.unwrap());
}

#[tokio::test]
async fn sonarr_creates_a_category_with_no_save_path() {
    let fixture = fixture!("sonarr_creates_a_category_with_no_save_path");
    let app = TestApp::no_torrents();
    let session = app.login().await;

    let response = send(&app.router(), &fixture.request, Some(&session)).await;

    assert_eq!(response.status().as_u16(), fixture.response.status);
    assert_eq!(body_text(response).await, fixture.response.body.unwrap_or_default());
}

#[tokio::test]
async fn the_category_is_stored_with_an_empty_save_path() {
    let fixture = fixture!("the_category_is_stored_with_an_empty_save_path");
    let app = TestApp::no_torrents();
    let session = app.login().await;
    app.create_tv_sonarr_category(&session).await;

    let response = send(&app.router(), &fixture.request, Some(&session)).await;

    assert_eq!(response.status().as_u16(), fixture.response.status);
    let actual: Value = serde_json::from_str(&body_text(response).await).unwrap();
    let expected: Value = serde_json::from_str(&fixture.response.body.unwrap()).unwrap();
    assert_eq!(actual, expected);
}

#[tokio::test]
async fn torrents_info_is_empty_for_a_fresh_category() {
    let fixture = fixture!("torrents_info_is_empty_for_a_fresh_category");
    let app = TestApp::no_torrents();
    let session = app.login().await;
    app.create_tv_sonarr_category(&session).await;

    let response = send(&app.router(), &fixture.request, Some(&session)).await;

    assert_eq!(response.status().as_u16(), fixture.response.status);
    assert_eq!(body_text(response).await, fixture.response.body.unwrap());
}

/// The regression test for the bug this whole recording exists to have
/// found: before the fix in `qbit::torrents::destination_for`, an empty
/// category save path was taken literally, and every real add through
/// Sonarr or Radarr failed with a permission error trying to write to the
/// server's own working directory. See `harness/README.md`.
#[tokio::test]
async fn sonarr_adds_a_magnet_with_a_category_and_no_save_path() {
    let fixture = fixture!("sonarr_adds_a_magnet_with_a_category_and_no_save_path");
    let app = TestApp::no_torrents();
    let session = app.login().await;
    app.create_tv_sonarr_category(&session).await;

    let router = app.router();
    let response = send(&router, &fixture.request, Some(&session)).await;

    assert_eq!(response.status().as_u16(), fixture.response.status);
    assert_eq!(body_text(response).await, fixture.response.body.unwrap());

    for _ in 0..200 {
        if app.state.engine.snapshot().iter().any(|s| s.torrent.is_some()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let list_response = send(
        &router,
        &FixtureRequest {
            method: "GET".into(),
            path: "/api/v2/torrents/info?category=tv-sonarr".into(),
            content_type: None,
            body: None,
            authenticated: true,
        },
        Some(&session),
    )
    .await;
    let list: Value = serde_json::from_str(&body_text(list_response).await).unwrap();
    assert_eq!(
        list[0]["save_path"], "/downloads/tv-sonarr",
        "an empty category save path must resolve under the download directory, not to it literally"
    );
}

/// `added_on`, `dlspeed` and `eta` are wall clock and network dependent and
/// are not asserted on here: see the fixture's own note. What is asserted is
/// the field the underlying bug actually broke, `save_path` and
/// `content_path`, plus everything else this handler can compute the same
/// way regardless of when or how fast the transfer ran.
#[tokio::test]
async fn torrents_info_while_downloading() {
    let hash = "dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c";
    let progress = TorrentProgress {
        progress: Progress {
            downloaded: 50_000_000,
            total: Some(276_445_467),
            bytes_per_sec: 32_222_153,
            smoothed_bytes_per_sec: 32_222_153,
        },
        status: TorrentStatus { info_hash: Some(hash.to_string()), ..Default::default() },
        name: Some("Big Buck Bunny".to_string()),
        seeding: false,
        phase: None,
    };
    let app = TestApp::with_backend(FakeTorrentBackend::new(progress));
    let session = app.login().await;
    app.create_tv_sonarr_category(&session).await;
    let magnet = format!("magnet:?xt=urn:btih:{hash}&dn=Big+Buck+Bunny");
    let id = app
        .state
        .engine
        .add(DownloadSpec::new(magnet, app.state.config.download_dir.join("tv-sonarr")));
    // `engine.add` alone is not the same request Sonarr sent: the real
    // `torrents/add` handler is what attaches a category label, and that
    // label, not the destination passed here, is what `torrents/info`
    // filters on. Setting it directly is the same effect for a test that is
    // about the listing rather than about `add` itself.
    app.state.engine.set_labels(
        id,
        std::collections::BTreeMap::from([("category".to_string(), "tv-sonarr".to_string())]),
    );
    for _ in 0..200 {
        if app.state.engine.snapshot().iter().any(|s| s.torrent.is_some()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let router = app.router();
    let list_response = send(
        &router,
        &FixtureRequest {
            method: "GET".into(),
            path: "/api/v2/torrents/info?category=tv-sonarr".into(),
            content_type: None,
            body: None,
            authenticated: true,
        },
        Some(&session),
    )
    .await;
    let list: Value = serde_json::from_str(&body_text(list_response).await).unwrap();
    let entry = &list[0];

    let fixture = fixture!("torrents_info_while_downloading");
    let recorded: Value = serde_json::from_str(&fixture.response.body.unwrap()).unwrap();
    let recorded = &recorded[0];
    for field in ["hash", "name", "size", "category", "state", "num_seeds", "num_leechs"] {
        assert_eq!(entry[field], recorded[field], "field {field} diverged from the recording");
    }
    assert_eq!(entry["save_path"], "/downloads/tv-sonarr");
    assert_eq!(entry["content_path"], "/downloads/tv-sonarr");
}

/// Unlike the in-progress fixture, every field here except `added_on` and
/// `completion_on` is reproducible: a finished transfer's `eta` and
/// `dlspeed` settle to fixed values rather than a live measurement, so this
/// test asserts the whole recorded body but those two fields.
#[tokio::test]
async fn torrents_info_after_completion() {
    let hash = "dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c";
    let progress = TorrentProgress {
        progress: Progress {
            downloaded: 276_445_467,
            total: Some(276_445_467),
            bytes_per_sec: 0,
            smoothed_bytes_per_sec: 0,
        },
        status: TorrentStatus { info_hash: Some(hash.to_string()), ..Default::default() },
        name: Some("Big Buck Bunny".to_string()),
        seeding: true,
        phase: None,
    };
    let app = TestApp::with_backend(FakeTorrentBackend::new(progress));
    let session = app.login().await;
    app.create_tv_sonarr_category(&session).await;
    let magnet = format!("magnet:?xt=urn:btih:{hash}&dn=Big+Buck+Bunny");
    let id = app
        .state
        .engine
        .add(DownloadSpec::new(magnet, app.state.config.download_dir.join("tv-sonarr")));
    // `engine.add` alone is not the same request Sonarr sent: the real
    // `torrents/add` handler is what attaches a category label, and that
    // label, not the destination passed here, is what `torrents/info`
    // filters on. Setting it directly is the same effect for a test that is
    // about the listing rather than about `add` itself.
    app.state.engine.set_labels(
        id,
        std::collections::BTreeMap::from([("category".to_string(), "tv-sonarr".to_string())]),
    );
    for _ in 0..200 {
        if app.state.engine.snapshot().iter().any(|s| s.state == State::Seeding) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let fixture = fixture!("torrents_info_after_completion");
    let response = send(&app.router(), &fixture.request, Some(&session)).await;
    assert_eq!(response.status().as_u16(), fixture.response.status);
    let actual: Value = serde_json::from_str(&body_text(response).await).unwrap();
    let mut expected: Value = serde_json::from_str(&fixture.response.body.unwrap()).unwrap();

    for field in ["added_on", "completion_on"] {
        actual.as_array().unwrap()[0].as_object().unwrap().get(field).unwrap();
        expected[0][field] = actual[0][field].clone();
    }
    assert_eq!(actual, expected);
}
