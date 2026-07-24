//! Shared Link-header parsing for GitHub pagination.

/// Extract the `rel="next"` URL out of a raw Link header. Returns
/// `None` when no `rel="next"` is present (signalling the last
/// page) or when the URL cannot be parsed. Exposed as `pub` so
/// the test suite can drive it directly without a network fixture.
pub fn next_url_from_link_header(header: &str) -> Option<String> {
    for segment in header.split(',') {
        let segment = segment.trim();
        let mut parts = segment.split(';');
        let url_part = parts.next()?.trim();
        let url = url_part
            .strip_prefix('<')
            .and_then(|s| s.strip_suffix('>'))?;
        for rel in parts {
            let rel = rel.trim();
            if rel == "rel=\"next\"" {
                return Some(url.to_string());
            }
        }
    }
    None
}
