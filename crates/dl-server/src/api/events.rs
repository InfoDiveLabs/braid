//! `GET /api/v1/events`: the whole transfer list, pushed at 1 Hz.
//!
//! One `data:` frame carrying every transfer plus the combined totals, rather
//! than a per-transfer event each time something changes. The list is small
//! (a handful of rows even on a busy box), the UI redraws the whole thing
//! from state on every frame anyway, and a diffing protocol earns nothing at
//! this size beyond a second implementation of the same bug the plain one
//! would have had.

use crate::state::AppState;
use axum::Router;
use axum::extract::State;
use axum::response::sse::{Event, Sse};
use axum::routing::get;
use dl_core::engine::Engine;
use futures_util::stream::Stream;
use serde_json::json;
use std::convert::Infallible;
use std::time::{Duration, Instant};
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::IntervalStream;

/// How often the list is even considered for sending.
///
/// A rate a person can watch a number change at without it feeling laggy, and
/// slow enough that ten open browser tabs cost ten cheap wakeups a second
/// rather than ten expensive ones.
const TICK: Duration = Duration::from_secs(1);

/// How long a frame may go unchanged before a comment goes out anyway.
///
/// Not for the browser, which does not care whether anything arrives while
/// nothing is happening: it is for whatever sits between the two. Plenty of
/// reverse proxies and load balancers close a connection that has carried
/// nothing for around thirty seconds, and a comment line is the cheapest
/// thing that resets that clock without pretending a change occurred.
const KEEPALIVE_AFTER: Duration = Duration::from_secs(15);

pub(super) fn routes() -> Router<AppState> {
    Router::new().route("/api/v1/events", get(stream_events))
}

fn frame_json(engine: &Engine) -> serde_json::Value {
    let snapshots = engine.snapshot();
    json!({
        "transfers": snapshots.iter().map(super::v1::transfer_json).collect::<Vec<_>>(),
        "total_bytes_per_sec": engine.total_bytes_per_sec(),
    })
}

/// What to put on the wire for one tick, if anything.
///
/// Separated from the stream itself so the fifteen-second rule can be
/// exercised against a clock a test controls, rather than one it would have
/// to sleep through.
struct Ticker {
    last_frame: Option<String>,
    last_sent: Instant,
}

impl Ticker {
    fn new(now: Instant) -> Self {
        Self { last_frame: None, last_sent: now }
    }

    fn tick(&mut self, frame: String, now: Instant) -> Option<Event> {
        if self.last_frame.as_deref() != Some(frame.as_str()) {
            self.last_frame = Some(frame.clone());
            self.last_sent = now;
            return Some(Event::default().data(frame));
        }
        if now.duration_since(self.last_sent) >= KEEPALIVE_AFTER {
            self.last_sent = now;
            // A bare comment: SSE ignores a line starting with `:`, so this
            // reaches the browser as nothing at all, which is exactly the
            // point. It exists for the proxy watching the byte stream, not
            // for whatever is parsing `data:` frames on the other end.
            return Some(Event::default().comment("keepalive"));
        }
        None
    }
}

async fn stream_events(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut ticker = Ticker::new(Instant::now());
    let stream = IntervalStream::new(tokio::time::interval(TICK)).filter_map(move |_| {
        let frame = frame_json(&state.engine).to_string();
        ticker.tick(frame, Instant::now()).map(Ok)
    });
    Sse::new(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_tick_always_has_something_to_say() {
        let start = Instant::now();
        let mut ticker = Ticker::new(start);
        assert!(ticker.tick("{}".into(), start).is_some());
    }

    #[test]
    fn an_unchanged_frame_sends_nothing_until_the_keepalive_falls_due() {
        let start = Instant::now();
        let mut ticker = Ticker::new(start);
        ticker.tick("{}".into(), start);

        let quiet = ticker.tick("{}".into(), start + Duration::from_secs(5));
        assert!(quiet.is_none(), "nothing changed, and it has not been fifteen seconds");

        let keepalive = ticker.tick("{}".into(), start + Duration::from_secs(16));
        assert!(keepalive.is_some(), "fifteen seconds of silence must produce a comment");
    }

    #[test]
    fn a_real_change_is_sent_immediately_regardless_of_the_keepalive_clock() {
        let start = Instant::now();
        let mut ticker = Ticker::new(start);
        ticker.tick("{\"a\":1}".into(), start);

        let changed = ticker.tick("{\"a\":2}".into(), start + Duration::from_millis(200));
        assert!(changed.is_some(), "a real change must not wait for the keepalive interval");
    }

    #[test]
    fn a_keepalive_resets_its_own_clock_rather_than_firing_every_tick() {
        let start = Instant::now();
        let mut ticker = Ticker::new(start);
        ticker.tick("{}".into(), start);
        assert!(ticker.tick("{}".into(), start + Duration::from_secs(16)).is_some());

        // Immediately after, nothing has changed and no fifteen seconds have
        // passed since that comment: firing again every tick would make the
        // keepalive itself the thing keeping the connection busy.
        let again = ticker.tick("{}".into(), start + Duration::from_secs(17));
        assert!(again.is_none());
    }

    #[tokio::test]
    async fn the_route_answers_as_server_sent_events() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode, header};
        use dl_core::budget::Budget;
        use dl_core::engine::{DownloadSpec, EngineConfig, SourceFactory};
        use dl_core::lane::LaneSet;
        use std::sync::Arc;
        use tower::ServiceExt;

        struct NoSources;
        impl SourceFactory for NoSources {
            fn lanes_for(&self, _spec: &DownloadSpec) -> dl_core::Result<Box<dyn LaneSet>> {
                unreachable!("this test starts nothing")
            }
        }

        let engine = Engine::new(Arc::new(NoSources), EngineConfig::default(), Budget::unlimited());
        let state = AppState { engine, config: Arc::new(crate::config::Config::default()) };
        let app = routes().with_state(state);

        let response = app
            .oneshot(Request::builder().uri("/api/v1/events").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers().get(header::CONTENT_TYPE).unwrap(), "text/event-stream");
    }
}
