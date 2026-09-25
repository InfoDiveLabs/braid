//! Authentication, landing before there is anything to authenticate.
//!
//! This exists before the qBittorrent-compatible API does, on purpose: adding
//! a password check under handlers that already work is how one of them gets
//! forgotten. Writing the middleware first and the routes after means every
//! route is born inside it, and the test at the bottom of this file exists to
//! keep it that way as more routes are added later by people who never read
//! this comment.
//!
//! The session store lives here rather than beside the future login endpoint
//! because the qBittorrent-compatible login is going to hand out exactly the
//! same kind of session this module already needs for the plain web UI. One
//! store, shared, rather than two that have to agree with each other forever.

use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use axum::extract::{Extension, Request};
use axum::http::{HeaderMap, HeaderName, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::Mutex;

/// The only account this server knows about.
///
/// A name check rather than a hard-coded skip is what lets `verify` share a
/// signature with the qBittorrent login endpoint it will back later, which
/// takes a username whether or not this server ever has more than one.
const USERNAME: &str = "admin";

/// A password long enough to type off a screen but too long to fall to an
/// offline guess of the whole keyspace: 16 random bytes is 128 bits, and
/// base32 turns that into something a person can actually retype.
const PASSWORD_BYTES: usize = 16;

/// 32 random bytes per session id: enough that guessing one by brute force is
/// not a strategy, for a value that lives as long as a browser tab stays open.
const SESSION_ID_BYTES: usize = 32;

/// The cookie name is not a style choice: the qBittorrent-compatible layer
/// that will share this session store is written against exactly this name,
/// because that is what real qBittorrent clients send.
pub(crate) const SESSION_COOKIE: &str = "SID";

/// One line of OS randomness, base32-encoded.
///
/// Reusing `argon2`'s own salt generator rather than adding a general-purpose
/// RNG crate means the password and every session id are backed by the same,
/// already-audited call into the OS random source that `Argon2::hash_password`
/// itself relies on for salting: one trusted source of randomness in this
/// binary, not a second one bolted on beside it. It only ever hands back 16
/// bytes at a time, so longer values are built by calling it as many times as
/// needed.
fn fill_random(buf: &mut [u8]) {
    let mut filled = 0;
    while filled < buf.len() {
        let chunk = argon2::password_hash::generate_salt();
        let take = chunk.len().min(buf.len() - filled);
        buf[filled..filled + take].copy_from_slice(&chunk[..take]);
        filled += take;
    }
}

/// RFC 4648 base32, upper-case, unpadded.
///
/// Base32 rather than base64 or hex because the whole point of the password
/// is that somebody reads it off a terminal, possibly out loud, and types it
/// into a login form on another device: no `0`/`O` or `1`/`l` confusion, and
/// no punctuation that a phone keyboard buries behind a long-press.
fn base32_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let mut buffer: u32 = 0;
    let mut bits = 0u32;
    for &byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buffer >> bits) & 0x1f) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buffer << (5 - bits)) & 0x1f) as usize] as char);
    }
    out
}

fn generate_password() -> String {
    let mut bytes = [0u8; PASSWORD_BYTES];
    fill_random(&mut bytes);
    base32_encode(&bytes)
}

/// The credentials file's only key. A single constant rather than a struct
/// field name that could drift from the string literal used to parse it back.
const CREDENTIALS_KEY: &str = "admin";

fn parse_hash(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .find_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim(), value.trim()))
        .filter(|(key, _)| *key == CREDENTIALS_KEY)
        .map(|(_, value)| value.to_string())
}

fn write_credentials_file(path: &Path, hash: &str) -> io::Result<()> {
    let contents = format!(
        "# Generated once, on first start. The password itself is never written\n\
         # here: only its Argon2id hash. It is printed to the startup log instead,\n\
         # exactly once. Delete this file to have a new one generated.\n\
         {CREDENTIALS_KEY} = {hash}\n"
    );
    fs::write(path, contents)?;

    // Nothing here is secret that the hash alone would give away quickly, but
    // a credentials file the rest of the container can read is one more thing
    // an unrelated bug in some other process could leak.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(path, perms)?;
    }

    Ok(())
}

fn hash_password(password: &str) -> io::Result<String> {
    argon2::Argon2::default()
        .hash_password(password.as_bytes())
        .map(|hash| hash.to_string())
        .map_err(|e| io::Error::other(format!("could not hash the generated password: {e}")))
}

/// The one account this server has, and what it takes to prove you are it.
#[derive(Clone, Debug)]
pub struct Credentials {
    hash: String,
}

impl Credentials {
    /// Load `credentials.conf` from `dir`, or create it with a freshly
    /// generated password if it is not there.
    ///
    /// The second half of the return value is that password, and it is
    /// `Some` exactly once: the one moment it exists in memory at all. A
    /// shipped default password is how these containers end up on the public
    /// internet with somebody else's torrents in them, and generating one
    /// that is never written down anywhere but the log defends against the
    /// same failure a different way: there is no default to have shipped.
    pub fn load_or_create(dir: &Path) -> io::Result<(Credentials, Option<String>)> {
        let path = dir.join("credentials.conf");

        if let Ok(text) = fs::read_to_string(&path) {
            return match parse_hash(&text) {
                Some(hash) => Ok((Credentials { hash }, None)),
                // The file exists but does not parse. Generating a new
                // password and silently overwriting it would throw away
                // whatever an operator meant by editing it; better to fail
                // loudly than to lock them out of an account they think they
                // still control.
                None => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} does not contain a recognisable credential", path.display()),
                )),
            };
        }

        let password = generate_password();
        let hash = hash_password(&password)?;
        write_credentials_file(&path, &hash)?;
        Ok((Credentials { hash }, Some(password)))
    }

    /// Check a login attempt. Only `admin` exists, so any other name is
    /// rejected without touching the hash at all: hashing is the expensive
    /// step, and there is no username here worth spending it on.
    pub fn verify(&self, username: &str, password: &str) -> bool {
        if username != USERNAME {
            return false;
        }
        let Ok(parsed) = PasswordHash::new(&self.hash) else { return false };
        argon2::Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok()
    }
}

/// Print the generated password where `docker logs` will show it, once, and
/// nowhere else. Blank lines on both sides are not decoration: a password
/// buried on the same line as a timestamp and a log level is a password
/// somebody greps past.
pub fn announce_generated_password(password: &str) {
    tracing::info!(
        "\n\n    A password was generated for this server. It will not be shown again:\n\n        {password}\n\n"
    );
}

/// Call once at startup with `Config::auth_required`. Skipping the login
/// check the moment somebody sets this is one honest choice; the operator
/// forgetting six months later that they ever made it is not, so this warns
/// on every single start rather than once.
pub fn warn_if_disabled(auth_required: bool) {
    if !auth_required {
        tracing::warn!(
            "auth_required=false: every request is being accepted without a password. \
             Only use this on a network nothing untrusted can reach."
        );
    }
}

/// Live sessions, by id.
///
/// A `HashSet` of ids rather than a map to anything else, because nothing
/// about a session is looked up by its id except whether it is still one: no
/// expiry, no per-session data. The qBittorrent-compatible login this will
/// back one day shares this exact store, which is why it lives beside
/// `Credentials` instead of next to whatever handler issues the plain web
/// UI's cookie.
#[derive(Default)]
pub struct Sessions {
    active: Mutex<HashSet<String>>,
}

impl Sessions {
    /// Start a new session and return its id.
    pub fn issue(&self) -> String {
        let mut bytes = [0u8; SESSION_ID_BYTES];
        fill_random(&mut bytes);
        let id = base32_encode(&bytes);
        self.active.lock().unwrap().insert(id.clone());
        id
    }

    pub fn valid(&self, id: &str) -> bool {
        self.active.lock().unwrap().contains(id)
    }

    /// End a session immediately. Logging out has to mean logging out: a
    /// session that merely started counting down to expiry would still work
    /// for anyone who had it, for as long as that countdown ran.
    pub fn revoke(&self, id: &str) {
        self.active.lock().unwrap().remove(id);
    }
}

/// The `Set-Cookie` value for a freshly issued session.
///
/// `HttpOnly` and `SameSite=Strict` always: nothing about serving this
/// content to a browser tab needs script access to the cookie, and nothing
/// about this API is meant to be driven from a page on another origin.
///
/// `secure` is the caller's job to determine, not this function's: most
/// people terminate TLS at a reverse proxy in front of this process, so
/// whether the *browser's* connection was secure is not something a look at
/// this process's own socket can answer. Setting `Secure` unconditionally
/// would mean the cookie is silently never returned over a plain-HTTP
/// deployment, which shows up as "login does nothing" with no error anywhere
/// to explain why.
pub fn session_cookie(id: &str, secure: bool) -> String {
    let mut cookie = format!("{SESSION_COOKIE}={id}; HttpOnly; SameSite=Strict; Path=/");
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// Whether the request reaching this process arrived over TLS, as reported by
/// a reverse proxy. There is no other way to know: a proxy that terminates
/// TLS speaks plain HTTP to whatever is behind it, so this process's own
/// socket looks identical either way.
pub fn request_is_secure(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("https"))
}

/// Whether a request claiming to come from `origin` may act on a server
/// answering to `host`.
///
/// Without this, any page open in the same browser as this server's web UI
/// can drive it: an `<img>` tag or a background `fetch` on a page that has
/// nothing to do with this project, riding the session cookie the browser
/// attaches automatically. Comparing `Origin` to the `Host` the request
/// itself claims to be for catches exactly that, without needing this server
/// to know its own public address in advance.
///
/// A request with no `Origin` header at all is not a browser making a
/// cross-site request: browsers attach it themselves and a script cannot
/// remove it, so its absence means a plain HTTP client, which this check has
/// nothing useful to say about.
pub fn origin_allowed(origin: Option<&str>, host: &str) -> bool {
    match origin {
        None => true,
        Some(origin) => {
            let claimed_host = origin.split_once("://").map_or(origin, |(_, rest)| rest);
            claimed_host.eq_ignore_ascii_case(host)
        }
    }
}

fn header_str(headers: &HeaderMap, name: HeaderName) -> Option<&str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// Pull one cookie's value out of a raw `Cookie` header. Written by hand
/// rather than pulled in as a dependency for the one line of parsing it
/// takes: a `key=value` pair separated by `; `.
pub(crate) fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let raw = header_str(headers, header::COOKIE)?;
    raw.split(';').find_map(|pair| {
        let (key, value) = pair.trim().split_once('=')?;
        (key == name).then_some(value)
    })
}

/// Reject anything that is not a same-origin request carrying a live session,
/// with `403 Forbidden`.
///
/// Not `401`: this is what qBittorrent's own API returns for an
/// unauthenticated request, and the compatible layer this will eventually sit
/// beside has clients already written expecting exactly that code.
///
/// Sessions arrive through `Extension` rather than `State`, so this
/// middleware can be layered onto a router whose own state is anything at
/// all: the plain web UI and the qBittorrent-compatible API will not
/// necessarily share one `State` type, but both need to share this store.
pub async fn require_auth(
    Extension(sessions): Extension<std::sync::Arc<Sessions>>,
    request: Request,
    next: Next,
) -> Response {
    let origin = header_str(request.headers(), header::ORIGIN);
    let host = header_str(request.headers(), header::HOST).unwrap_or("");
    if !origin_allowed(origin, host) {
        return StatusCode::FORBIDDEN.into_response();
    }

    let authenticated =
        cookie_value(request.headers(), SESSION_COOKIE).is_some_and(|id| sessions.valid(id));
    if !authenticated {
        return StatusCode::FORBIDDEN.into_response();
    }

    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::routing::get;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    #[test]
    fn a_password_is_generated_on_first_start_and_never_again() {
        // A shipped default password is how these containers end up on the public
        // internet with somebody else's torrents in them.
        let dir = tempfile::tempdir().unwrap();
        let (first, shown) = Credentials::load_or_create(dir.path()).unwrap();
        let password = shown.expect("the generated password must be shown once");
        assert!(password.len() >= 12);

        let (second, shown_again) = Credentials::load_or_create(dir.path()).unwrap();
        assert!(shown_again.is_none(), "it must not be regenerated on restart");
        assert!(second.verify("admin", &password));
        assert!(!first.verify("admin", "wrong"));
    }

    #[test]
    fn the_stored_file_does_not_contain_the_password() {
        let dir = tempfile::tempdir().unwrap();
        let (_, shown) = Credentials::load_or_create(dir.path()).unwrap();
        let on_disk = std::fs::read_to_string(dir.path().join("credentials.conf")).unwrap();
        assert!(!on_disk.contains(&shown.unwrap()));
    }

    #[test]
    fn a_revoked_session_stops_working_immediately() {
        let s = Sessions::default();
        let id = s.issue();
        assert!(s.valid(&id));
        s.revoke(&id);
        assert!(!s.valid(&id), "logging out must end the session, not expire it later");
    }

    #[test]
    fn a_request_from_another_origin_is_refused() {
        // Without this, any page the user visits can drive their download client.
        assert!(!origin_allowed(Some("https://evil.test"), "localhost:8080"));
        assert!(origin_allowed(Some("http://localhost:8080"), "localhost:8080"));
        assert!(
            origin_allowed(None, "localhost:8080"),
            "a client that sends none is not a browser"
        );
    }

    /// Send one request through a real socket rather than calling the router
    /// in-process: driving a `Router` directly means reaching for the
    /// `tower-service` crate's `Service` trait, which is a dependency this
    /// file would otherwise not need at all just to drive a test, for a
    /// project that already treats every dependency as a cost to justify.
    /// `axum::serve` is already a dependency for the real server to run on.
    async fn status_for(app: &Router, path: &str, session: Option<&str>) -> StatusCode {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = app.clone();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut stream = TcpStream::connect(addr).await.unwrap();
        let cookie_line = session.map(|id| format!("Cookie: SID={id}\r\n")).unwrap_or_default();
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n{cookie_line}\r\n"
        );
        stream.write_all(request.as_bytes()).await.unwrap();

        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        server.abort();

        let text = String::from_utf8_lossy(&response);
        let status_line = text.lines().next().unwrap_or("");
        let code: u16 =
            status_line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        StatusCode::from_u16(code).unwrap()
    }

    #[tokio::test]
    async fn a_route_added_later_and_left_unprotected_is_caught() {
        // The failure this exists for is not an endpoint written wrong today, it
        // is one added in six months by somebody who did not know the middleware
        // had to be applied. Build a router with the middleware, then assert every
        // route on it refuses an unauthenticated request.
        let sessions = Arc::new(Sessions::default());
        let id = sessions.issue();

        let app = Router::new()
            .route("/existing", get(|| async { "ok" }))
            .route("/added/later", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn(require_auth))
            .layer(Extension(sessions));

        for path in ["/existing", "/added/later"] {
            assert_eq!(
                status_for(&app, path, None).await,
                StatusCode::FORBIDDEN,
                "{path} answered a request with no session"
            );
            assert_eq!(
                status_for(&app, path, Some(&id)).await,
                StatusCode::OK,
                "{path} refused a request with a valid session"
            );
        }
    }
}

/// The way in.
///
/// Outside the middleware, necessarily: a client with no session cannot be
/// asked for one in order to get one. These are the only two routes on the
/// server that answer without a cookie, which is why they live here beside the
/// check rather than among the transfer endpoints, where a reader would have
/// to notice the exemption.
///
/// The qBittorrent-compatible API will grow its own login later. It issues
/// into this same store rather than a second one, so a client that logged in
/// one way is not mysteriously unauthenticated the other.
pub fn routes() -> axum::Router<crate::state::AppState> {
    axum::Router::new()
        .route("/api/v1/login", axum::routing::post(login))
        .route("/api/v1/logout", axum::routing::post(logout))
}

#[derive(serde::Deserialize)]
struct Login {
    username: String,
    password: String,
}

async fn login(
    Extension(sessions): Extension<std::sync::Arc<Sessions>>,
    Extension(credentials): Extension<std::sync::Arc<Credentials>>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Login>,
) -> Response {
    if !credentials.verify(&body.username, &body.password) {
        // Deliberately not distinguishing a wrong name from a wrong password,
        // which would tell an attacker which half to keep guessing at.
        return (StatusCode::FORBIDDEN, "wrong username or password").into_response();
    }
    let id = sessions.issue();
    let cookie = session_cookie(&id, request_is_secure(&headers));
    match header::HeaderValue::from_str(&cookie) {
        Ok(value) => ([(header::SET_COOKIE, value)], StatusCode::NO_CONTENT).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn logout(
    Extension(sessions): Extension<std::sync::Arc<Sessions>>,
    headers: HeaderMap,
) -> Response {
    // Revoked rather than left to expire: somebody logging out on a shared
    // machine means it now, and a session that outlives the request is exactly
    // what they were trying to prevent.
    if let Some(id) = cookie_value(&headers, SESSION_COOKIE) {
        sessions.revoke(id);
    }
    StatusCode::NO_CONTENT.into_response()
}
