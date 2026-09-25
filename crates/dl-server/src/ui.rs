//! The web UI: plain HTML, CSS and JavaScript, compiled into the binary.
//!
//! No bundler, no framework, no Node: the project's whole pitch is a small,
//! self-contained executable, and pulling a JavaScript toolchain into CI to
//! draw a table and a form would spend that on a screen that does not need
//! it. The cost lands here instead, as more hand-written markup than a
//! framework would ask for; that trade is deliberate, not an oversight.
//!
//! `main.rs` merges [`routes`] into its own router once it exists there:
//! this module has no `mod` line of its own yet for the reason `api::mod`
//! gives for the same situation, which is also why the same allowance
//! follows below.
#![allow(dead_code)]

use axum::Router;
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use std::sync::OnceLock;

const INDEX_TEMPLATE: &str = include_str!("../ui/index.html");
const APP_CSS: &str = include_str!("../ui/app.css");
const APP_JS: &str = include_str!("../ui/app.js");

/// This build's own version, so an upgrade is never served last build's
/// script out of a cache. `index.html` links to `/app.css` and `/app.js`
/// with this appended as a query string; the two routes answer with a cache
/// lifetime long enough that the string is the only thing that ever changes
/// about those URLs, and a new one is exactly what forces a browser (or a
/// reverse proxy sitting in front of this process) to ask again.
const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn routes() -> Router<crate::state::AppState> {
    Router::new().route("/", get(index)).route("/app.css", get(css)).route("/app.js", get(js))
}

/// `index.html` with `{{VERSION}}` filled in, built once rather than on every
/// request: the template never changes after compilation, so re-running the
/// same substitution for every visitor would be pure waste.
fn index_html() -> &'static str {
    static RENDERED: OnceLock<String> = OnceLock::new();
    RENDERED.get_or_init(|| INDEX_TEMPLATE.replace("{{VERSION}}", VERSION))
}

async fn index() -> Response {
    // Never cached: this is the one response that decides which versioned
    // `/app.css` and `/app.js` URLs a browser asks for next. Caching it would
    // mean an upgraded binary keeps handing out a page that still names the
    // previous build's assets.
    (
        [(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"))],
        index_html(),
    )
        .into_response()
}

/// A year, and `immutable`: safe only because the URL this is served at
/// carries the build's version and changes the moment the content would.
/// Browsers already treat a cached `immutable` response as never worth a
/// conditional request until it expires, which is the whole point of paying
/// for a version string in the first place.
const LONG_CACHE: HeaderValue = HeaderValue::from_static("public, max-age=31536000, immutable");

async fn css() -> Response {
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("text/css; charset=utf-8")),
            (header::CACHE_CONTROL, LONG_CACHE),
        ],
        APP_CSS,
    )
        .into_response()
}

async fn js() -> Response {
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("text/javascript; charset=utf-8")),
            (header::CACHE_CONTROL, LONG_CACHE),
        ],
        APP_JS,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use dl_core::Engine;
    use dl_core::budget::Budget;
    use dl_core::engine::{DownloadSpec, EngineConfig, SourceFactory};
    use dl_core::lane::LaneSet;
    use http_body_util::BodyExt;
    use std::sync::Arc;
    use tower::ServiceExt;

    struct NoSources;
    impl SourceFactory for NoSources {
        fn lanes_for(&self, _spec: &DownloadSpec) -> dl_core::Result<Box<dyn LaneSet>> {
            unreachable!("this test starts nothing")
        }
    }

    fn app() -> Router {
        let engine = Engine::new(Arc::new(NoSources), EngineConfig::default(), Budget::unlimited());
        let state = crate::state::AppState { engine, config: Arc::new(crate::config::Config::default()) };
        routes().with_state(state)
    }

    async fn body_text(response: axum::http::Response<Body>) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn the_index_page_answers_as_html_and_names_the_built_assets() {
        let response =
            app().oneshot(Request::builder().uri("/").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
        let body = body_text(response).await;
        // Proof the template was actually filled in, not served with the
        // literal placeholder still sitting in it.
        assert!(!body.contains("{{VERSION}}"));
        assert!(body.contains(&format!("v={VERSION}")));
    }

    #[tokio::test]
    async fn the_stylesheet_and_script_answer_with_their_real_content_types() {
        let css = app()
            .clone()
            .oneshot(Request::builder().uri("/app.css").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(css.headers().get(header::CONTENT_TYPE).unwrap(), "text/css; charset=utf-8");
        assert!(css.headers().get(header::CACHE_CONTROL).unwrap().to_str().unwrap().contains("immutable"));

        let js = app()
            .oneshot(Request::builder().uri("/app.js").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(js.headers().get(header::CONTENT_TYPE).unwrap(), "text/javascript; charset=utf-8");
    }
}
