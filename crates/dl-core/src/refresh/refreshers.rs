//! The three ways a link gets re-resolved.

use crate::error::{Error, Result};
use crate::refresh::json::Json;
use crate::refresh::model::{RefreshCtx, ResolvedSource};
use crate::refresh::{LinkRefresher, expiry};
use std::sync::Arc;
use std::time::Duration;

/// Hands back the URL the download started with.
///
/// The default, and not a no-op: a transient 403 is retried, an expiry stated
/// in the URL still drives proactive refresh, and the refresh cap is what
/// stops a link that has genuinely died.
pub struct StaticRefresher;

#[async_trait::async_trait]
impl LinkRefresher for StaticRefresher {
    fn id(&self) -> &str {
        "static"
    }

    async fn resolve(&self, ctx: &RefreshCtx) -> Result<ResolvedSource> {
        Ok(ResolvedSource::new(ctx.original_url.clone()))
    }
}

/// Runs a user-supplied command and reads the fresh source from its stdout.
///
/// This is the yt-dlp hook, and it is arbitrary code execution. It is never
/// constructed from a URL, a config discovered on disk, or anything an origin
/// can influence: only from an explicit request by the person running the
/// download. The command is executed directly, never through a shell, so a URL
/// containing shell metacharacters cannot become a second command.
pub struct CommandRefresher {
    program: String,
    args: Vec<String>,
    timeout: Duration,
}

impl CommandRefresher {
    /// Split a command line into a program and its arguments.
    ///
    /// `{url}` is replaced by the URL being refreshed; with no placeholder the
    /// URL is appended as the final argument, which is what `yt-dlp -g` wants.
    pub fn parse(command: &str) -> Result<Self> {
        let words = split_words(command)
            .ok_or_else(|| Error::Transport(format!("unbalanced quotes in {command:?}")))?;
        let mut words = words.into_iter();
        let program =
            words.next().ok_or_else(|| Error::Transport("the refresh command is empty".into()))?;
        Ok(Self { program, args: words.collect(), timeout: Duration::from_secs(60) })
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn argv(&self, url: &str) -> Vec<String> {
        let substituted: Vec<String> = self.args.iter().map(|a| a.replace("{url}", url)).collect();
        if self.args.iter().any(|a| a.contains("{url}")) {
            substituted
        } else {
            let mut all = substituted;
            all.push(url.to_string());
            all
        }
    }
}

#[async_trait::async_trait]
impl LinkRefresher for CommandRefresher {
    fn id(&self) -> &str {
        "command"
    }

    async fn resolve(&self, ctx: &RefreshCtx) -> Result<ResolvedSource> {
        let argv = self.argv(&ctx.original_url);
        let output = tokio::time::timeout(
            self.timeout,
            tokio::process::Command::new(&self.program)
                .args(&argv)
                .stdin(std::process::Stdio::null())
                .output(),
        )
        .await
        .map_err(|_| Error::Transport(format!("the refresh command {:?} timed out", self.program)))?
        .map_err(|e| Error::Transport(format!("could not run {:?}: {e}", self.program)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Transport(format!(
                "the refresh command {:?} exited with {}: {}",
                self.program,
                output.status,
                stderr.trim()
            )));
        }

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        parse_reply(&stdout)
    }
}

/// Re-resolves by calling an HTTP endpoint and reading a URL out of the reply.
pub struct ApiRefresher {
    http: Arc<dyn HttpJson>,
    endpoint: String,
    method: String,
    headers: Vec<(String, String)>,
    body: Option<String>,
    url_path: String,
    headers_path: Option<String>,
    expires_path: Option<String>,
}

impl ApiRefresher {
    /// `url_path` is a dotted path into the reply, such as `data.stream.href`.
    pub fn get(
        http: Arc<dyn HttpJson>,
        endpoint: impl Into<String>,
        url_path: impl Into<String>,
    ) -> Self {
        Self {
            http,
            endpoint: endpoint.into(),
            method: "GET".into(),
            headers: Vec::new(),
            body: None,
            url_path: url_path.into(),
            headers_path: None,
            expires_path: None,
        }
    }

    pub fn post(
        http: Arc<dyn HttpJson>,
        endpoint: impl Into<String>,
        body: impl Into<String>,
        url_path: impl Into<String>,
    ) -> Self {
        let mut refresher = Self::get(http, endpoint, url_path);
        refresher.method = "POST".into();
        refresher.body = Some(body.into());
        refresher
    }

    pub fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.headers = headers;
        self
    }

    /// Where the reply states a lifetime in seconds, if it does.
    pub fn with_expires_path(mut self, path: impl Into<String>) -> Self {
        self.expires_path = Some(path.into());
        self
    }

    pub fn with_headers_path(mut self, path: impl Into<String>) -> Self {
        self.headers_path = Some(path.into());
        self
    }
}

#[async_trait::async_trait]
impl LinkRefresher for ApiRefresher {
    fn id(&self) -> &str {
        "api"
    }

    async fn resolve(&self, ctx: &RefreshCtx) -> Result<ResolvedSource> {
        let endpoint = self.endpoint.replace("{url}", &percent_encode(&ctx.original_url));
        let body = self.body.as_ref().map(|b| b.replace("{url}", &ctx.original_url));
        let reply =
            self.http.request(&self.method, &endpoint, &self.headers, body.as_deref()).await?;

        let value = Json::parse(&reply).map_err(|e| {
            Error::Transport(format!("the refresh endpoint returned invalid json: {e}"))
        })?;

        let url = value
            .get(&self.url_path)
            .and_then(Json::as_str)
            .ok_or_else(|| {
                Error::Transport(format!("no url at {:?} in the refresh reply", self.url_path))
            })?
            .to_string();

        let headers = self
            .headers_path
            .as_deref()
            .and_then(|path| value.get(path))
            .and_then(Json::as_string_pairs)
            .unwrap_or_default();

        let mut resolved = ResolvedSource::new(url).with_headers(headers);
        if let Some(seconds) = self
            .expires_path
            .as_deref()
            .and_then(|path| value.get(path))
            .and_then(Json::as_f64)
            .filter(|s| *s > 0.0)
        {
            resolved = resolved.expiring_in(Duration::from_secs_f64(seconds));
        }
        Ok(resolved)
    }
}

/// The one thing a refresher needs from the network.
///
/// `dl-core` does not speak HTTP; an implementation is supplied by `dl-net`,
/// which keeps the engine testable with no sockets.
#[async_trait::async_trait]
pub trait HttpJson: Send + Sync {
    async fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&str>,
    ) -> Result<String>;
}

/// Read a refresher's stdout: a `{url, headers}` object, or a bare URL, which
/// is what `yt-dlp -g` prints.
fn parse_reply(stdout: &str) -> Result<ResolvedSource> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err(Error::Transport("the refresh command printed nothing".into()));
    }

    if !trimmed.starts_with('{') {
        let url = trimmed
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .ok_or_else(|| Error::Transport("the refresh command printed no url".into()))?;
        return Ok(with_derived_expiry(ResolvedSource::new(url)));
    }

    let value = Json::parse(trimmed)
        .map_err(|e| Error::Transport(format!("the refresh command printed invalid json: {e}")))?;
    let url = value
        .get("url")
        .and_then(Json::as_str)
        .ok_or_else(|| Error::Transport("the refresh reply has no \"url\"".into()))?;
    let headers = value.get("headers").and_then(Json::as_string_pairs).unwrap_or_default();

    let mut resolved = ResolvedSource::new(url).with_headers(headers);
    if let Some(seconds) = value.get("expires_in").and_then(Json::as_f64).filter(|s| *s > 0.0) {
        resolved = resolved.expiring_in(Duration::from_secs_f64(seconds));
    } else {
        resolved = with_derived_expiry(resolved);
    }
    Ok(resolved)
}

fn with_derived_expiry(mut resolved: ResolvedSource) -> ResolvedSource {
    if let Some(lifetime) = expiry::lifetime_of(&resolved.url, &resolved.headers) {
        resolved = resolved.expiring_in(lifetime);
    }
    resolved
}

/// Split on whitespace, honouring single and double quotes. Returns `None` on
/// an unbalanced quote rather than guessing where the argument ended.
fn split_words(input: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;

    for c in input.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => current.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                started = true;
            }
            None if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            None => {
                current.push(c);
                started = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if started {
        words.push(current);
    }
    Some(words)
}

/// Encode a URL for inclusion in another URL's query string.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refresh::model::{RefreshReason, SourceKey};

    fn ctx(url: &str) -> RefreshCtx {
        RefreshCtx {
            key: SourceKey::new("s", 0),
            original_url: url.to_string(),
            current: None,
            reason: RefreshReason::Rejected,
            attempt: 0,
        }
    }

    #[tokio::test]
    async fn the_static_refresher_returns_the_original_url() {
        let resolved = StaticRefresher.resolve(&ctx("http://x.test/f")).await.unwrap();
        assert_eq!(resolved.url, "http://x.test/f");
    }

    #[test]
    fn a_command_line_is_split_without_a_shell() {
        assert_eq!(
            split_words(r#"yt-dlp -g --cookies "my cookies.txt""#).unwrap(),
            vec!["yt-dlp", "-g", "--cookies", "my cookies.txt"]
        );
        // An empty quoted argument is a real argument.
        assert_eq!(split_words(r#"a "" b"#).unwrap(), vec!["a", "", "b"]);
        assert_eq!(split_words(r#"a "unbalanced"#), None);
    }

    #[test]
    fn a_url_is_passed_as_one_argument_however_it_is_spelled() {
        // Passed as argv, so metacharacters in the URL are inert. Through a
        // shell this would be a second command.
        let hostile = "http://x.test/f?a=1;rm -rf /";
        let refresher = CommandRefresher::parse("resolver --json").unwrap();
        assert_eq!(refresher.argv(hostile), vec!["--json", hostile]);

        let placed = CommandRefresher::parse("resolver --url {url} --json").unwrap();
        assert_eq!(placed.argv(hostile), vec!["--url", hostile, "--json"]);
    }

    #[test]
    fn a_bare_url_on_stdout_is_accepted() {
        // What `yt-dlp -g` prints.
        let resolved = parse_reply("\n https://cdn.test/v.mp4?sig=abc \n").unwrap();
        assert_eq!(resolved.url, "https://cdn.test/v.mp4?sig=abc");
        assert!(resolved.headers.is_empty());
    }

    #[test]
    fn a_json_reply_carries_headers_and_a_lifetime() {
        let resolved = parse_reply(
            r#"{"url":"https://cdn.test/v.mp4","headers":{"Cookie":"s=1"},"expires_in":300}"#,
        )
        .unwrap();
        assert_eq!(resolved.url, "https://cdn.test/v.mp4");
        assert_eq!(resolved.headers, vec![("Cookie".to_string(), "s=1".to_string())]);
        let lifetime = resolved.expires_at.unwrap().duration_since(resolved.issued_at);
        assert_eq!(lifetime.as_secs(), 300);
    }

    #[test]
    fn a_signed_url_on_stdout_gets_its_lifetime_read_from_the_query_string() {
        let resolved = parse_reply("https://b.s3.test/k?X-Amz-Expires=900").unwrap();
        assert!(resolved.expires_at.is_some(), "the lifetime in the url was ignored");
    }

    #[test]
    fn nonsense_on_stdout_is_an_error_rather_than_a_fabricated_url() {
        assert!(parse_reply("").is_err());
        assert!(parse_reply("   ").is_err());
        assert!(parse_reply("{ not json").is_err());
        assert!(parse_reply(r#"{"no_url": 1}"#).is_err());
    }

    struct CannedJson(String);

    #[async_trait::async_trait]
    impl HttpJson for CannedJson {
        async fn request(
            &self,
            _method: &str,
            _url: &str,
            _headers: &[(String, String)],
            _body: Option<&str>,
        ) -> Result<String> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn an_api_reply_is_read_by_json_path() {
        let http = Arc::new(CannedJson(
            r#"{"data":{"href":"https://cdn.test/v.mp4"},"ttl":120,"hdrs":{"Referer":"https://x.test/"}}"#
                .to_string(),
        ));
        let refresher = ApiRefresher::get(http, "https://api.test/resolve?u={url}", "data.href")
            .with_expires_path("ttl")
            .with_headers_path("hdrs");

        let resolved = refresher.resolve(&ctx("http://x.test/f")).await.unwrap();
        assert_eq!(resolved.url, "https://cdn.test/v.mp4");
        assert_eq!(resolved.headers, vec![("Referer".to_string(), "https://x.test/".to_string())]);
        assert_eq!(resolved.expires_at.unwrap().duration_since(resolved.issued_at).as_secs(), 120);
    }

    #[tokio::test]
    async fn an_api_reply_missing_the_url_is_an_error() {
        let http = Arc::new(CannedJson(r#"{"error":"gone"}"#.to_string()));
        let refresher = ApiRefresher::get(http, "https://api.test/resolve", "data.href");
        assert!(refresher.resolve(&ctx("http://x.test/f")).await.is_err());
    }

    #[test]
    fn a_url_substituted_into_an_endpoint_is_encoded() {
        assert_eq!(
            percent_encode("http://x.test/a b?c=1&d=2"),
            "http%3A%2F%2Fx.test%2Fa%20b%3Fc%3D1%26d%3D2"
        );
    }
}
