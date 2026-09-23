//! A [`ByteSource`] that re-resolves its link instead of dying at 403.
//!
//! Staleness is judged before the transport's own verdict is surfaced: a login
//! page arrives as a `200` to a ranged request, which looks exactly like "the
//! resource changed" and is not.

use crate::error::{Error, Result};
use crate::model::SourceInfo;
use crate::refresh::handle::SourceHandle;
use crate::refresh::model::{RefreshReason, ResolvedSource, ResponseSummary, Staleness};
use crate::source::{ByteSource, ByteStream, Fetch};
use std::sync::{Arc, Mutex};

/// One request's result, with enough of the response to judge staleness even
/// when the transport refused it.
pub struct Attempt<T> {
    pub summary: ResponseSummary,
    /// `Err` when the transport refused the response. The summary is still
    /// filled in, because the refusal may be a symptom of expiry.
    pub outcome: Result<T>,
}

impl<T> Attempt<T> {
    pub fn ok(summary: ResponseSummary, value: T) -> Self {
        Self { summary, outcome: Ok(value) }
    }

    pub fn refused(summary: ResponseSummary, error: Error) -> Self {
        Self { summary, outcome: Err(error) }
    }
}

/// Fetches whatever a [`ResolvedSource`] currently points at.
///
/// The outer `Result` is for failures with no response at all; a response the
/// transport rejected comes back as [`Attempt::refused`] so staleness can be
/// judged first.
#[async_trait::async_trait]
pub trait ResolvedFetcher: Send + Sync {
    async fn probe(&self, source: &ResolvedSource) -> Result<Attempt<SourceInfo>>;

    async fn open(
        &self,
        source: &ResolvedSource,
        fetch: Fetch,
        expected_content_type: Option<&str>,
    ) -> Result<Attempt<ByteStream>>;
}

pub struct RefreshingSource {
    handle: Arc<SourceHandle>,
    fetcher: Arc<dyn ResolvedFetcher>,
    /// What this resource looked like when it last worked. An HTML body cannot
    /// be called an error page without it.
    expected_content_type: Mutex<Option<String>>,
}

impl RefreshingSource {
    pub fn new(handle: Arc<SourceHandle>, fetcher: Arc<dyn ResolvedFetcher>) -> Self {
        Self { handle, fetcher, expected_content_type: Mutex::new(None) }
    }

    pub fn handle(&self) -> &Arc<SourceHandle> {
        &self.handle
    }

    /// Run one request, refreshing and retrying for as long as the link is
    /// stale and the refresh budget allows.
    ///
    /// The loop terminates because every iteration that does not return calls
    /// `get_fresh`, and `get_fresh` either consumes budget or observes a
    /// generation another task paid for. The budget is finite and shared, so
    /// the total number of iterations across all lanes is bounded by it.
    async fn with_refresh<T, F, Fut>(&self, mut attempt: F) -> Result<T>
    where
        T: Send,
        F: FnMut(Arc<ResolvedSource>, Option<String>) -> Fut + Send,
        Fut: std::future::Future<Output = Result<Attempt<T>>> + Send,
    {
        let mut ambiguous_refreshes = 0u32;
        loop {
            let current = self.handle.current_or_proactive().await;
            let generation = current.generation;
            let expected = self.expected_content_type.lock().unwrap().clone();

            let attempted = attempt(Arc::clone(&current), expected).await?;
            let verdict = self.handle.refresher().staleness(&attempted.summary);

            let reason = match verdict {
                Staleness::Fresh => return attempted.outcome,
                // A 404 that survives one refresh is a 404. Refreshing again
                // would spend the budget denying that a file was deleted.
                Staleness::Ambiguous if ambiguous_refreshes > 0 => return attempted.outcome,
                Staleness::Ambiguous => {
                    ambiguous_refreshes += 1;
                    RefreshReason::Ambiguous
                }
                Staleness::Stale => RefreshReason::Rejected,
            };

            tracing::debug!(
                key = %self.handle.key(),
                status = attempted.summary.status,
                ?verdict,
                "the link looks stale; re-resolving"
            );
            if let Err(e) = self.handle.get_fresh(generation, reason).await {
                return Err(explain(e, attempted.outcome.err(), attempted.summary.status));
            }
        }
    }

    fn remember_content_type(&self, info: &SourceInfo) {
        if let Some(content_type) = &info.content_type {
            let mut expected = self.expected_content_type.lock().unwrap();
            if expected.is_none() {
                *expected = Some(content_type.clone());
            }
        }
    }
}

#[async_trait::async_trait]
impl ByteSource for RefreshingSource {
    async fn probe(&self) -> Result<SourceInfo> {
        let fetcher = Arc::clone(&self.fetcher);
        let info = self
            .with_refresh(move |source, _expected| {
                let fetcher = Arc::clone(&fetcher);
                async move { fetcher.probe(&source).await }
            })
            .await?;
        self.remember_content_type(&info);
        Ok(info)
    }

    async fn open(&self, fetch: Fetch) -> Result<ByteStream> {
        let fetcher = Arc::clone(&self.fetcher);
        self.with_refresh(move |source, expected| {
            let fetcher = Arc::clone(&fetcher);
            let fetch = fetch.clone();
            async move { fetcher.open(&source, fetch, expected.as_deref()).await }
        })
        .await
    }
}

/// Report why the download stopped, keeping what the origin last said.
///
/// Without it a dead link surfaces as "reached the limit of 20 refreshes" with
/// no hint that the origin had been answering 403 all along.
fn explain(refresh_error: Error, refused: Option<Error>, status: u16) -> Error {
    let Error::RefreshExhausted { detail } = &refresh_error else {
        return refresh_error;
    };
    let last = match refused {
        Some(e) => e.to_string(),
        None => format!("HTTP {status}"),
    };
    Error::RefreshExhausted { detail: format!("{detail}; the origin last said: {last}") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refresh::handle::RefreshBudget;
    use crate::refresh::model::{RefreshCtx, RefreshPolicy, SourceKey};
    use crate::refresh::{LinkRefresher, StaticRefresher};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Serves bytes only for the newest URL the refresher minted.
    struct Expiring {
        live_url: Mutex<String>,
        requests: AtomicU64,
        rejections: AtomicU64,
    }

    impl Expiring {
        fn summary(
            &self,
            status: u16,
            content_type: &str,
            expected: Option<&str>,
        ) -> ResponseSummary {
            ResponseSummary {
                status,
                content_type: Some(content_type.to_string()),
                expected_content_type: expected.map(str::to_string),
                ranged: false,
                ..Default::default()
            }
        }
    }

    #[async_trait::async_trait]
    impl ResolvedFetcher for Expiring {
        async fn probe(&self, source: &ResolvedSource) -> Result<Attempt<SourceInfo>> {
            self.requests.fetch_add(1, Ordering::SeqCst);
            if *self.live_url.lock().unwrap() != source.url {
                self.rejections.fetch_add(1, Ordering::SeqCst);
                return Ok(Attempt::refused(
                    self.summary(403, "text/plain", None),
                    Error::Http { status: 403 },
                ));
            }
            let info = SourceInfo {
                len: Some(1000),
                accept_ranges: true,
                content_type: Some("application/octet-stream".into()),
                final_url: source.url.clone(),
                ..Default::default()
            };
            Ok(Attempt::ok(self.summary(206, "application/octet-stream", None), info))
        }

        async fn open(
            &self,
            source: &ResolvedSource,
            _fetch: Fetch,
            expected: Option<&str>,
        ) -> Result<Attempt<ByteStream>> {
            self.requests.fetch_add(1, Ordering::SeqCst);
            if *self.live_url.lock().unwrap() != source.url {
                self.rejections.fetch_add(1, Ordering::SeqCst);
                return Ok(Attempt::refused(
                    self.summary(403, "text/plain", expected),
                    Error::Http { status: 403 },
                ));
            }
            let stream: ByteStream =
                Box::pin(futures_util::stream::once(async { Ok(bytes::Bytes::from_static(b"x")) }));
            Ok(Attempt::ok(self.summary(206, "application/octet-stream", expected), stream))
        }
    }

    /// Mints a new URL each time, and tells the origin which one is live.
    struct Rotating {
        fetcher: Arc<Expiring>,
        minted: AtomicU64,
    }

    #[async_trait::async_trait]
    impl LinkRefresher for Rotating {
        fn id(&self) -> &str {
            "rotating"
        }
        async fn resolve(&self, ctx: &RefreshCtx) -> Result<ResolvedSource> {
            let n = self.minted.fetch_add(1, Ordering::SeqCst) + 1;
            let url = format!("{}?token={n}", ctx.original_url);
            *self.fetcher.live_url.lock().unwrap() = url.clone();
            Ok(ResolvedSource::new(url))
        }
    }

    fn rotating() -> (Arc<Expiring>, RefreshingSource) {
        let fetcher = Arc::new(Expiring {
            live_url: Mutex::new(String::new()),
            requests: AtomicU64::new(0),
            rejections: AtomicU64::new(0),
        });
        let refresher =
            Arc::new(Rotating { fetcher: Arc::clone(&fetcher), minted: AtomicU64::new(0) });
        let handle = SourceHandle::new(
            SourceKey::new("s", 0),
            "http://x.test/f",
            refresher,
            RefreshPolicy::default(),
            RefreshBudget::new(20),
        );
        let source =
            RefreshingSource::new(handle, Arc::clone(&fetcher) as Arc<dyn ResolvedFetcher>);
        (fetcher, source)
    }

    #[tokio::test]
    async fn a_rejected_link_is_re_resolved_and_the_request_succeeds() {
        let (fetcher, source) = rotating();
        let info = source.probe().await.expect("a refreshable link should recover from 403");
        assert_eq!(info.len, Some(1000));
        assert_eq!(fetcher.rejections.load(Ordering::SeqCst), 1);
        assert_eq!(source.handle().resolves(), 1, "one 403 should cost one resolution");
    }

    #[tokio::test]
    async fn a_link_that_never_works_fails_within_the_cap() {
        // StaticRefresher keeps handing back the same dead URL; without a cap
        // this is an infinite loop.
        let fetcher = Arc::new(Expiring {
            live_url: Mutex::new("http://nowhere.test/".into()),
            requests: AtomicU64::new(0),
            rejections: AtomicU64::new(0),
        });
        let policy = RefreshPolicy { max_refreshes: 5, ..Default::default() };
        let handle = SourceHandle::new(
            SourceKey::new("s", 0),
            "http://x.test/f",
            Arc::new(StaticRefresher),
            policy,
            RefreshBudget::new(policy.max_refreshes),
        );
        let source =
            RefreshingSource::new(handle, Arc::clone(&fetcher) as Arc<dyn ResolvedFetcher>);

        let err = source.probe().await.expect_err("a permanently dead link must fail");
        assert!(matches!(err, Error::RefreshExhausted { .. }), "{err:?}");
        assert!(err.to_string().contains("403"), "the origin's answer was lost: {err}");
        assert_eq!(fetcher.requests.load(Ordering::SeqCst), policy.max_refreshes as u64 + 1);
        assert!(!err.invalidates_partial_data(), "a dead link must not discard finished chunks");
    }

    /// The case status codes cannot see.
    #[tokio::test]
    async fn a_login_page_returned_with_200_is_treated_as_expiry() {
        struct Portal {
            served: AtomicU64,
        }

        #[async_trait::async_trait]
        impl ResolvedFetcher for Portal {
            async fn probe(&self, _source: &ResolvedSource) -> Result<Attempt<SourceInfo>> {
                let info = SourceInfo {
                    len: Some(1000),
                    accept_ranges: true,
                    content_type: Some("application/octet-stream".into()),
                    ..Default::default()
                };
                Ok(Attempt::ok(
                    ResponseSummary {
                        status: 206,
                        content_type: Some("application/octet-stream".into()),
                        ..Default::default()
                    },
                    info,
                ))
            }

            async fn open(
                &self,
                _source: &ResolvedSource,
                _fetch: Fetch,
                expected: Option<&str>,
            ) -> Result<Attempt<ByteStream>> {
                // Every length and range check passes; only the media type
                // says this is a web page and not the file.
                let n = self.served.fetch_add(1, Ordering::SeqCst);
                let body = if n == 0 { &b"<html>login</html>"[..] } else { &b"x"[..] };
                let stream: ByteStream = Box::pin(futures_util::stream::once(async move {
                    Ok(bytes::Bytes::from_static(body))
                }));
                Ok(Attempt::ok(
                    ResponseSummary {
                        status: 200,
                        content_type: Some(
                            if n == 0 {
                                "text/html; charset=utf-8"
                            } else {
                                "application/octet-stream"
                            }
                            .to_string(),
                        ),
                        expected_content_type: expected.map(str::to_string),
                        ranged: true,
                        ..Default::default()
                    },
                    stream,
                ))
            }
        }

        let fetcher = Arc::new(Portal { served: AtomicU64::new(0) });
        let handle = SourceHandle::new(
            SourceKey::new("s", 0),
            "http://x.test/f",
            Arc::new(StaticRefresher),
            RefreshPolicy::default(),
            RefreshBudget::new(20),
        );
        let source = RefreshingSource::new(handle, fetcher);

        source.probe().await.expect("the probe establishes what the resource is");
        let _body =
            source.open(Fetch::whole()).await.expect("the second attempt returns the real body");
        assert_eq!(source.handle().resolves(), 1, "the html body was accepted as content");
    }

    #[tokio::test]
    async fn a_404_is_refreshed_once_and_then_believed() {
        struct AlwaysMissing {
            requests: AtomicU64,
        }

        #[async_trait::async_trait]
        impl ResolvedFetcher for AlwaysMissing {
            async fn probe(&self, _source: &ResolvedSource) -> Result<Attempt<SourceInfo>> {
                self.requests.fetch_add(1, Ordering::SeqCst);
                Ok(Attempt::refused(
                    ResponseSummary { status: 404, ..Default::default() },
                    Error::Http { status: 404 },
                ))
            }
            async fn open(
                &self,
                _source: &ResolvedSource,
                _fetch: Fetch,
                _expected: Option<&str>,
            ) -> Result<Attempt<ByteStream>> {
                unreachable!("the probe fails first")
            }
        }

        let fetcher = Arc::new(AlwaysMissing { requests: AtomicU64::new(0) });
        let handle = SourceHandle::new(
            SourceKey::new("s", 0),
            "http://x.test/f",
            Arc::new(StaticRefresher),
            RefreshPolicy::default(),
            RefreshBudget::new(20),
        );
        let source =
            RefreshingSource::new(handle, Arc::clone(&fetcher) as Arc<dyn ResolvedFetcher>);

        let err = source.probe().await.expect_err("a deleted file must still fail");
        assert!(matches!(err, Error::Http { status: 404 }), "{err:?}");
        assert_eq!(
            fetcher.requests.load(Ordering::SeqCst),
            2,
            "a 404 costs one refresh, not twenty"
        );
    }
}
