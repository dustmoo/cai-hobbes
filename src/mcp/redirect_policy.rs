//! Redirect handling for reqwest clients that carry credentials.
//!
//! reqwest's default policy follows up to 10 redirects to any host and strips
//! only `Authorization`, `Cookie` and friends on a cross-origin hop — not
//! custom credential headers such as Composio's `x-api-key` (the class of
//! GHSA-9g45-5xwm-f3wc). Any client that sends such a header uses
//! [`same_origin_redirect_policy`].

/// The most redirects followed for one request.
pub const MAX_REDIRECTS: usize = 10;

/// Follow redirects only within the first request's origin. reqwest's default strips
/// Authorization/Cookie cross-origin but not custom headers such as x-api-key (GHSA-9g45 class).
///
/// A cross-origin redirect is not followed: the caller receives the 3xx
/// response itself.
pub fn same_origin_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        let same_origin = attempt
            .previous()
            .first()
            .is_some_and(|first| is_same_origin(first, attempt.url()));
        if !same_origin {
            tracing::warn!(
                "Refused a cross-origin redirect to {}",
                crate::log_utils::redact_url(attempt.url().as_str())
            );
            return attempt.stop();
        }
        if attempt.previous().len() > MAX_REDIRECTS {
            return attempt.error("too many redirects");
        }
        attempt.follow()
    })
}

/// Scheme, host and port (default ports made explicit; hosts are already
/// lower-cased by URL parsing).
fn is_same_origin(a: &url::Url, b: &url::Url) -> bool {
    let (a, b) = (a.origin(), b.origin());
    a.is_tuple() && a == b
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{any, header, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .redirect(same_origin_redirect_policy())
            .build()
            .unwrap()
    }

    fn same(a: &str, b: &str) -> bool {
        is_same_origin(&url::Url::parse(a).unwrap(), &url::Url::parse(b).unwrap())
    }

    #[test]
    fn origin_comparison() {
        let base = "https://backend.composio.dev/v3/mcp/abc";
        assert!(same(base, "https://backend.composio.dev/other?x=1"));
        assert!(same(base, "https://BACKEND.composio.dev:443/x"), "case and default port");
        assert!(!same(base, "https://evil.example/collect"));
        assert!(!same(base, "http://backend.composio.dev/x"), "a scheme downgrade");
        assert!(!same(base, "https://backend.composio.dev:8443/x"), "another port");
        assert!(!same(base, "https://composio.dev.evil.example/x"));
    }

    #[tokio::test]
    async fn cross_origin_redirect_is_not_followed() {
        let origin = MockServer::start().await;
        let other = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200))
            .mount(&other)
            .await;
        Mock::given(path("/start"))
            .respond_with(
                ResponseTemplate::new(307).insert_header("Location", format!("{}/steal", other.uri())),
            )
            .mount(&origin)
            .await;

        let resp = client()
            .get(format!("{}/start", origin.uri()))
            .header("x-api-key", "secret")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 307);
        assert!(other.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn same_origin_redirect_is_followed_with_headers() {
        let origin = MockServer::start().await;
        Mock::given(path("/start"))
            .respond_with(ResponseTemplate::new(307).insert_header("Location", "/next"))
            .mount(&origin)
            .await;
        Mock::given(path("/next"))
            .and(header("x-api-key", "secret"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&origin)
            .await;

        let resp = client()
            .get(format!("{}/start", origin.uri()))
            .header("x-api-key", "secret")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
    }

    #[tokio::test]
    async fn redirect_loops_error_out() {
        let origin = MockServer::start().await;
        Mock::given(path("/loop"))
            .respond_with(ResponseTemplate::new(307).insert_header("Location", "/loop"))
            .mount(&origin)
            .await;

        let err = client()
            .get(format!("{}/loop", origin.uri()))
            .send()
            .await
            .unwrap_err();
        assert!(err.is_redirect(), "{err}");
    }
}
