//! Lanes whose links are re-resolved when they stop working.
//!
//! Mirrors and interfaces have the same shape here: both are independent paths
//! to the same bytes, so both become lanes and go through the existing
//! selector rather than a second mechanism.

use crate::http::{HttpConfig, build_client, open_url, probe_url};
use crate::iface::Interface;
use crate::multi::{Binding, bound_client};
use dl_core::error::{Error, Result};
use dl_core::lane::LaneSet;
use dl_core::model::SourceInfo;
use dl_core::refresh::{
    Attempt, HttpJson, LinkRefresher, RefreshCoordinator, RefreshPolicy, RefreshingSource,
    ResolvedFetcher, ResolvedSource, SourceKey,
};
use dl_core::source::{ByteSource, ByteStream, Fetch};
use reqwest::Client;
use std::sync::Arc;

/// Fetches whatever a resolved source currently points at, over one client.
pub struct HttpFetcher {
    client: Client,
}

impl HttpFetcher {
    pub fn new(client: Client) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ResolvedFetcher for HttpFetcher {
    async fn probe(&self, source: &ResolvedSource) -> Result<Attempt<SourceInfo>> {
        probe_url(&self.client, &source.url, &source.headers).await
    }

    async fn open(
        &self,
        source: &ResolvedSource,
        fetch: Fetch,
        expected_content_type: Option<&str>,
    ) -> Result<Attempt<ByteStream>> {
        open_url(&self.client, &source.url, &source.headers, fetch, expected_content_type).await
    }
}

/// The engine's JSON-over-HTTP hook, for [`dl_core::ApiRefresher`].
pub struct ReqwestJson {
    client: Client,
}

impl ReqwestJson {
    pub fn new(client: Client) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl HttpJson for ReqwestJson {
    async fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&str>,
    ) -> Result<String> {
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|e| Error::Transport(format!("invalid http method {method:?}: {e}")))?;
        let mut request = self.client.request(method, url);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        if let Some(body) = body {
            request = request.body(body.to_string());
        }

        let response = request.send().await.map_err(|e| Error::Transport(e.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(Error::Http { status: status.as_u16() });
        }
        response.text().await.map_err(|e| Error::Transport(e.to_string()))
    }
}

/// One path to the bytes: a URL, optionally pinned to an interface.
#[derive(Clone, Debug)]
pub struct LaneSpec {
    pub label: String,
    /// Where this lane starts. Headers here accompany the first requests and
    /// are replaced wholesale when the link is re-resolved.
    pub initial: ResolvedSource,
    /// Headers this lane always sends, refresh requests included. This is
    /// where a lane's identity belongs, not in `initial`: a source-bound
    /// signature has to be re-issued to the same identity that will use it.
    pub headers: Vec<(String, String)>,
    pub interface: Option<Interface>,
}

impl LaneSpec {
    pub fn new(label: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            initial: ResolvedSource::new(url),
            headers: Vec::new(),
            interface: None,
        }
    }

    pub fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.headers = headers;
        self
    }

    pub fn on(mut self, interface: Interface) -> Self {
        self.label = format!("{} via {}", self.label, interface.name);
        self.interface = Some(interface);
        self
    }
}

/// Every combination of mirror and interface, as lanes.
///
/// A signature bound to the requesting address makes this a product rather
/// than a choice: each mirror has to be resolved separately on each interface.
pub fn lane_specs(urls: &[String], interfaces: &[Interface]) -> Vec<LaneSpec> {
    let mut specs = Vec::new();
    for (index, url) in urls.iter().enumerate() {
        let name = if index == 0 { "origin".to_string() } else { format!("mirror{index}") };
        if interfaces.is_empty() {
            specs.push(LaneSpec::new(name, url));
        } else {
            for interface in interfaces {
                specs.push(LaneSpec::new(name.clone(), url).on(interface.clone()));
            }
        }
    }
    specs
}

/// Chooses the refresher for one lane, given the client that lane will use.
///
/// A function rather than a single refresher because resolution can be bound
/// to the address that asks: a signature re-issued over the wrong interface is
/// rejected exactly like the one it replaced.
pub type RefresherFor<'a> = &'a dyn Fn(&LaneSpec, &Client) -> Option<Arc<dyn LinkRefresher>>;

/// A lane set whose sources re-resolve their links.
pub struct RefreshingLanes {
    sources: Vec<RefreshingSource>,
    labels: Vec<String>,
    bindings: Vec<Binding>,
    coordinator: Arc<RefreshCoordinator>,
}

impl RefreshingLanes {
    /// Build one lane per spec.
    ///
    /// `make_refresher` is given the lane's own client so a refresher can send
    /// its request over the same path as the chunks it resolves for; returning
    /// `None` falls back to the default.
    pub fn build(
        specs: Vec<LaneSpec>,
        config: &HttpConfig,
        policy: RefreshPolicy,
        default_refresher: Arc<dyn LinkRefresher>,
        make_refresher: RefresherFor<'_>,
    ) -> Result<Self> {
        if specs.is_empty() {
            return Err(Error::Transport("no usable network path is available".into()));
        }
        let coordinator = RefreshCoordinator::new(default_refresher, policy);

        let mut sources = Vec::with_capacity(specs.len());
        let mut labels = Vec::with_capacity(specs.len());
        let mut bindings = Vec::with_capacity(specs.len());

        for (lane, spec) in specs.iter().enumerate() {
            let (client, binding) = client_for(spec, config)?;
            let key = SourceKey::new(&spec.initial.url, lane);
            let handle =
                coordinator.handle_with(key, spec.initial.clone(), make_refresher(spec, &client));
            sources.push(RefreshingSource::new(handle, Arc::new(HttpFetcher::new(client))));
            labels.push(spec.label.clone());
            bindings.push(binding);
        }

        Ok(Self { sources, labels, bindings, coordinator })
    }

    /// The common case: one refresher for every lane, no interface pinning.
    pub fn mirrors(
        urls: &[String],
        config: &HttpConfig,
        policy: RefreshPolicy,
        refresher: Arc<dyn LinkRefresher>,
    ) -> Result<Self> {
        Self::build(lane_specs(urls, &[]), config, policy, refresher, &|_, _| None)
    }

    pub fn coordinator(&self) -> &Arc<RefreshCoordinator> {
        &self.coordinator
    }

    /// How many times a link was actually re-resolved, across every lane.
    pub fn resolves(&self) -> u64 {
        self.coordinator.resolves()
    }

    pub fn bindings(&self) -> &[Binding] {
        &self.bindings
    }
}

impl std::fmt::Debug for RefreshingLanes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RefreshingLanes")
            .field("lanes", &self.labels)
            .field("resolves", &self.resolves())
            .finish()
    }
}

impl LaneSet for RefreshingLanes {
    fn len(&self) -> usize {
        self.sources.len()
    }

    fn source(&self, lane: usize) -> &dyn ByteSource {
        &self.sources[lane]
    }

    fn label(&self, lane: usize) -> &str {
        &self.labels[lane]
    }
}

/// A client carrying this lane's constant headers, pinned if it has an
/// interface.
fn client_for(spec: &LaneSpec, config: &HttpConfig) -> Result<(Client, Binding)> {
    let mut config = config.clone();
    for (name, value) in &spec.headers {
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|e| Error::Transport(format!("invalid header name {name:?}: {e}")))?;
        let value = reqwest::header::HeaderValue::from_str(value)
            .map_err(|e| Error::Transport(format!("invalid value for header {name}: {e}")))?;
        config.headers.insert(name, value);
    }

    match &spec.interface {
        Some(interface) => bound_client(interface, &config),
        None => Ok((build_client(&config)?, Binding::Default)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dl_core::StaticRefresher;

    #[test]
    fn mirrors_and_interfaces_multiply_into_lanes() {
        // A signature bound to the requesting address is valid for one
        // (mirror, interface) pair only, so every pair needs its own lane.
        let interfaces = vec![
            Interface {
                name: "en0".into(),
                index: 1,
                ipv4: vec!["10.0.0.2".parse().unwrap()],
                ipv6: vec![],
                is_up: true,
                is_loopback: false,
                has_gateway: true,
                gateway_ipv4: None,
                kind: Default::default(),
                service_name: None,
            },
            Interface {
                name: "en5".into(),
                index: 2,
                ipv4: vec!["10.0.1.2".parse().unwrap()],
                ipv6: vec![],
                is_up: true,
                is_loopback: false,
                has_gateway: true,
                gateway_ipv4: None,
                kind: Default::default(),
                service_name: None,
            },
        ];
        let urls = vec!["http://a.test/f".to_string(), "http://b.test/f".to_string()];

        let specs = lane_specs(&urls, &interfaces);
        assert_eq!(specs.len(), 4);
        let labels: Vec<&str> = specs.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(
            labels,
            ["origin via en0", "origin via en5", "mirror1 via en0", "mirror1 via en5"]
        );

        // With no interfaces chosen, mirrors alone are the lanes.
        assert_eq!(lane_specs(&urls, &[]).len(), 2);
    }

    #[test]
    fn every_lane_gets_its_own_source_identity() {
        let urls = vec!["http://a.test/f".to_string(), "http://b.test/f".to_string()];
        let lanes = RefreshingLanes::mirrors(
            &urls,
            &HttpConfig::default(),
            RefreshPolicy::default(),
            Arc::new(StaticRefresher),
        )
        .unwrap();

        assert_eq!(lanes.len(), 2);
        assert_eq!(lanes.label(0), "origin");
        assert_eq!(lanes.label(1), "mirror1");
        assert_ne!(
            lanes.sources[0].handle().key(),
            lanes.sources[1].handle().key(),
            "two mirrors shared one resolution"
        );
    }

    #[test]
    fn a_lane_with_no_paths_is_refused_rather_than_silently_empty() {
        let err = RefreshingLanes::build(
            Vec::new(),
            &HttpConfig::default(),
            RefreshPolicy::default(),
            Arc::new(StaticRefresher),
            &|_, _| None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("no usable network path"), "{err}");
    }
}
