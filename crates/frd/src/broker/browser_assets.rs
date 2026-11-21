//! Browser-asset serving stub with strict Content-Security-Policy and headers (plan sections 5.2, 6).
//!
//! Serves versioned static assets for browser-based workstation access with:
//! - Strict CSP: `default-src 'self'; frame-ancestors 'none'; object-src 'none'; base-uri 'self'`
//! - Security headers: `X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY`
//! - Exact path validation with traversal prevention (`..`, backslashes, null bytes).

use core::fmt;

/// Content-Security-Policy header value for browser workstations.
pub const STRICT_CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; object-src 'none'; base-uri 'self'";

/// Response for a browser asset request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetResponse {
    /// HTTP status code.
    pub status_code: u16,
    /// MIME Content-Type header.
    pub content_type: &'static str,
    /// Content-Security-Policy header.
    pub csp: &'static str,
    /// Cache-Control header.
    pub cache_control: &'static str,
    /// X-Content-Type-Options header.
    pub x_content_type_options: &'static str,
    /// X-Frame-Options header.
    pub x_frame_options: &'static str,
    /// Body content bytes.
    pub body: Vec<u8>,
}

/// Typed errors in asset serving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetError {
    /// Attempted path traversal (e.g. `..` or leading `/../`).
    PathTraversalForbidden,
    /// Null bytes or control characters in URI path.
    InvalidCharacters,
    /// Path does not begin with `/`.
    InvalidPathFormat,
    /// Requested asset was not found.
    NotFound,
}

impl fmt::Display for AssetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PathTraversalForbidden => f.write_str("path traversal attempt forbidden"),
            Self::InvalidCharacters => f.write_str("invalid characters in requested path"),
            Self::InvalidPathFormat => f.write_str("path must begin with '/'"),
            Self::NotFound => f.write_str("browser asset not found"),
        }
    }
}

impl std::error::Error for AssetError {}

/// In-memory asset catalog.
#[derive(Debug, Default)]
pub struct BrowserAssets;

impl BrowserAssets {
    /// Check path safety: reject traversal (`..`), backslashes, null bytes.
    pub fn sanitize_path(raw_path: &str) -> Result<&str, AssetError> {
        if !raw_path.starts_with('/') {
            return Err(AssetError::InvalidPathFormat);
        }
        if raw_path.contains('\0') || raw_path.contains('\\') {
            return Err(AssetError::InvalidCharacters);
        }

        // Check path segments for directory traversal
        for segment in raw_path.split('/') {
            if segment == ".." {
                return Err(AssetError::PathTraversalForbidden);
            }
        }

        Ok(raw_path)
    }

    /// Serve an asset by request URI path.
    pub fn serve(path: &str) -> Result<AssetResponse, AssetError> {
        let clean_path = Self::sanitize_path(path)?;

        let (content_type, body) = match clean_path {
            "/" | "/index.html" => ("text/html; charset=utf-8", INDEX_HTML.as_bytes().to_vec()),
            "/app.js" | "/fr.js" => (
                "application/javascript; charset=utf-8",
                APP_JS.as_bytes().to_vec(),
            ),
            "/hevc_probe.js" => (
                "application/javascript; charset=utf-8",
                HEVC_PROBE_JS.as_bytes().to_vec(),
            ),
            "/wss_client.js" => (
                "application/javascript; charset=utf-8",
                WSS_CLIENT_JS.as_bytes().to_vec(),
            ),
            "/styles.css" | "/style.css" => {
                ("text/css; charset=utf-8", STYLES_CSS.as_bytes().to_vec())
            }
            "/manifest.json" => ("application/json", MANIFEST_JSON.as_bytes().to_vec()),
            _ => return Err(AssetError::NotFound),
        };

        Ok(AssetResponse {
            status_code: 200,
            content_type,
            csp: STRICT_CSP,
            cache_control: "no-cache, no-store, must-revalidate",
            x_content_type_options: "nosniff",
            x_frame_options: "DENY",
            body,
        })
    }
}

const INDEX_HTML: &str = include_str!("../../../../web/index.html");
const STYLES_CSS: &str = include_str!("../../../../web/styles.css");
const HEVC_PROBE_JS: &str = include_str!("../../../../web/hevc_probe.js");
const WSS_CLIENT_JS: &str = include_str!("../../../../web/wss_client.js");
const APP_JS: &str = include_str!("../../../../web/app.js");

const MANIFEST_JSON: &str = r#"{"name":"FrankenRemote","display":"fullscreen","start_url":"/"}"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serve_index_html_has_strict_headers() {
        let resp = BrowserAssets::serve("/").unwrap();
        assert_eq!(resp.status_code, 200);
        assert_eq!(resp.content_type, "text/html; charset=utf-8");
        assert_eq!(resp.csp, STRICT_CSP);
        assert_eq!(resp.x_frame_options, "DENY");
        assert_eq!(resp.x_content_type_options, "nosniff");
        assert!(resp.body.starts_with(b"<!DOCTYPE html>"));
    }

    #[test]
    fn serve_js_and_css() {
        for (path, expected_ct) in [
            ("/fr.js", "application/javascript; charset=utf-8"),
            ("/app.js", "application/javascript; charset=utf-8"),
            ("/hevc_probe.js", "application/javascript; charset=utf-8"),
            ("/wss_client.js", "application/javascript; charset=utf-8"),
            ("/style.css", "text/css; charset=utf-8"),
            ("/styles.css", "text/css; charset=utf-8"),
            ("/manifest.json", "application/json"),
        ] {
            let asset = BrowserAssets::serve(path).unwrap();
            assert_eq!(asset.content_type, expected_ct);
            assert_ne!(asset.body.len(), 0);
            assert!(
                asset.body.len() <= 64 * 1024,
                "browser asset {path} exceeds 64 KiB ceiling"
            );
            let text = String::from_utf8(asset.body).unwrap();
            assert!(
                !text.contains("http://") && !text.contains("https://") && !text.contains("cdn."),
                "browser asset {path} must remain 100% self-hosted without external URLs"
            );
        }
        let html = String::from_utf8(BrowserAssets::serve("/").unwrap().body).unwrap();
        assert!(!html.contains("http://") && !html.contains("https://"));
        assert!(html.contains("/hevc_probe.js"));
        assert!(html.contains("/wss_client.js"));
        assert!(html.contains("/app.js"));
    }

    #[test]
    fn path_traversal_is_strictly_forbidden() {
        assert_eq!(
            BrowserAssets::serve("/../etc/passwd"),
            Err(AssetError::PathTraversalForbidden)
        );
        assert_eq!(
            BrowserAssets::serve("/foo/../../bar"),
            Err(AssetError::PathTraversalForbidden)
        );
        assert_eq!(
            BrowserAssets::serve("/index.html/.."),
            Err(AssetError::PathTraversalForbidden)
        );
    }

    #[test]
    fn invalid_characters_are_rejected() {
        assert_eq!(
            BrowserAssets::serve("/foo\0bar"),
            Err(AssetError::InvalidCharacters)
        );
        assert_eq!(
            BrowserAssets::serve("/foo\\bar"),
            Err(AssetError::InvalidCharacters)
        );
    }

    #[test]
    fn non_slash_path_is_rejected() {
        assert_eq!(
            BrowserAssets::serve("index.html"),
            Err(AssetError::InvalidPathFormat)
        );
    }

    #[test]
    fn unknown_asset_returns_not_found() {
        assert_eq!(
            BrowserAssets::serve("/unknown_file.png"),
            Err(AssetError::NotFound)
        );
    }
}
