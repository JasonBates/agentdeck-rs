/// The browser UI embedded byte-for-byte in the executable.
///
/// HTTP routing is intentionally deferred; exposing the bytes here locks down
/// the packaging boundary without pretending the Rust server is implemented.
pub const INDEX_HTML: &str = include_str!("../../../Public/index.html");
pub const SETUP_HTML: &str = include_str!("../../../Public/docs/setup.html");

#[cfg(test)]
mod tests {
    use super::{INDEX_HTML, SETUP_HTML};

    #[test]
    fn existing_browser_is_embedded() {
        assert!(INDEX_HTML.starts_with("<!doctype html>"));
        assert!(INDEX_HTML.contains("fetch(BASE + path"));
        assert!(INDEX_HTML.contains("new EventSource(BASE + 'events')"));
    }

    #[test]
    fn setup_document_is_embedded() {
        assert!(SETUP_HTML.starts_with("<!doctype html>"));
        assert!(SETUP_HTML.contains("Recommended setup"));
    }
}
