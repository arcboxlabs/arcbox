//! Docker URI identity extraction helpers.

use axum::http::Uri;

/// Extracts the `{id}` token from an already-routed container request.
#[must_use]
pub fn extract_container_id(uri: &Uri) -> Option<String> {
    uri.path()
        .split_once("/containers/")?
        .1
        .split('/')
        .next()
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uri(s: &str) -> Uri {
        s.parse().unwrap()
    }

    #[test]
    fn extract_container_id_from_exec_subpath() {
        assert_eq!(
            extract_container_id(&uri("/containers/abc/exec")).as_deref(),
            Some("abc"),
        );
    }
}
