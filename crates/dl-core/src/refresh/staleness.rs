//! Deciding whether a link has stopped working.
//!
//! Status alone is not enough. The expiry that actually corrupts files is a
//! `200` carrying a login page, which every length and range check passes.

use crate::refresh::model::{ResponseSummary, Staleness};

/// The default heuristic every [`crate::refresh::LinkRefresher`] inherits.
pub fn classify(response: &ResponseSummary) -> Staleness {
    match response.status {
        // Credentials rejected, or the signer has withdrawn the object.
        401 | 403 | 410 => Staleness::Stale,
        // A 404 is genuinely ambiguous: an expired CDN path and a deleted file
        // look the same. Worth one refresh, not a retry loop.
        404 => Staleness::Ambiguous,
        200..=299 if is_error_page(response) => Staleness::Stale,
        _ => Staleness::Fresh,
    }
}

/// A successful status carrying HTML where the resource is not HTML.
///
/// The check needs what the resource looked like when it still worked: a
/// download of a web page is legitimately `text/html`, and no header
/// distinguishes it from the login form a portal substitutes. When the
/// expected type is unknown or is itself HTML, this stays silent rather than
/// refusing a download it cannot judge.
fn is_error_page(response: &ResponseSummary) -> bool {
    let Some(actual) = response.content_type.as_deref().map(media_type) else {
        return false;
    };
    let Some(expected) = response.expected_content_type.as_deref().map(media_type) else {
        return false;
    };
    is_html(actual) && !is_html(expected)
}

fn media_type(content_type: &str) -> &str {
    content_type.split(';').next().unwrap_or(content_type).trim()
}

fn is_html(media_type: &str) -> bool {
    media_type.eq_ignore_ascii_case("text/html")
        || media_type.eq_ignore_ascii_case("application/xhtml+xml")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(status: u16, actual: Option<&str>, expected: Option<&str>) -> ResponseSummary {
        ResponseSummary {
            status,
            content_type: actual.map(str::to_string),
            expected_content_type: expected.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn a_rejected_signature_is_stale() {
        for status in [401, 403, 410] {
            assert_eq!(classify(&summary(status, None, None)), Staleness::Stale, "{status}");
        }
    }

    #[test]
    fn a_404_is_ambiguous_rather_than_stale() {
        assert_eq!(classify(&summary(404, None, None)), Staleness::Ambiguous);
    }

    #[test]
    fn a_server_error_is_not_an_expired_link() {
        // Refreshing on a 503 would burn the refresh budget on an outage.
        for status in [408, 429, 500, 502, 503] {
            assert_eq!(classify(&summary(status, None, None)), Staleness::Fresh, "{status}");
        }
    }

    #[test]
    fn a_login_page_served_with_200_is_stale() {
        // The case that silently corrupts files: status, length and
        // Content-Range are all correct and the body is a web page.
        let response =
            summary(200, Some("text/html; charset=utf-8"), Some("application/octet-stream"));
        assert_eq!(classify(&response), Staleness::Stale);

        let partial = summary(206, Some("text/html"), Some("video/mp4"));
        assert_eq!(classify(&partial), Staleness::Stale);
    }

    #[test]
    fn downloading_an_html_page_is_not_mistaken_for_an_expired_link() {
        let response = summary(200, Some("text/html"), Some("text/html; charset=utf-8"));
        assert_eq!(classify(&response), Staleness::Fresh);
    }

    #[test]
    fn nothing_is_judged_before_the_resource_has_been_seen_working() {
        // With no expected type there is no evidence, and guessing would break
        // every download whose first response is HTML.
        assert_eq!(classify(&summary(200, Some("text/html"), None)), Staleness::Fresh);
    }

    #[test]
    fn a_normal_body_is_fresh() {
        let response =
            summary(206, Some("application/octet-stream"), Some("application/octet-stream"));
        assert_eq!(classify(&response), Staleness::Fresh);
    }
}
