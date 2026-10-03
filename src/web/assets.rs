//! Production assets are embedded at compile time; no runtime filesystem access.
use axum::{
    body::Body,
    http::{StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));
pub(super) async fn serve(uri: Uri) -> Response {
    let path = uri.path();
    let shell = matches!(path, "/" | "/login" | "/images" | "/activity" | "/about")
        || path
            .strip_prefix("/images/")
            .is_some_and(|id| id.parse::<u64>().is_ok());
    let target = if shell { "/index.html" } else { path };
    if let Some((name, bytes)) = ASSETS.iter().find(|(name, _)| *name == target) {
        let content_type = if name.ends_with(".html") {
            "text/html; charset=utf-8"
        } else if name.ends_with(".js") {
            "text/javascript; charset=utf-8"
        } else if name.ends_with(".css") {
            "text/css; charset=utf-8"
        } else if name.ends_with(".svg") {
            "image/svg+xml"
        } else {
            "application/octet-stream"
        };
        return Response::builder()
            .header(header::CONTENT_TYPE, content_type)
            .header(
                header::CACHE_CONTROL,
                if shell {
                    "no-cache"
                } else {
                    "public, max-age=31536000, immutable"
                },
            )
            .body(Body::from(*bytes))
            .unwrap();
    }
    if shell {
        return (StatusCode::SERVICE_UNAVAILABLE, "Frontend assets are not built. Run npm ci && npm run build in web/, then rebuild fauna-scan.").into_response();
    }
    super::WebError::not_found("The requested resource does not exist").into_response()
}

#[cfg(test)]
pub(super) fn available() -> bool {
    !ASSETS.is_empty()
}
