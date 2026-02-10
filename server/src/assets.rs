use rust_embed::RustEmbed;
use axum::{
    response::{IntoResponse, Response},
    http::{header, StatusCode, Uri},
    body::Body,
};

#[derive(RustEmbed)]
#[folder = "web"]
struct Assets;

pub async fn static_handler(uri: Uri) -> impl IntoResponse {
    let mut path = uri.path().trim_start_matches('/').to_string();

    if path.is_empty() {
        path = "index.html".to_string();
    }

    match Assets::get(&path) {
        Some(content) => {
            let mime = mime_guess::from_path(&path).first_or_octet_stream();
            (
                [(header::CONTENT_TYPE, mime.as_ref())],
                Body::from(content.data),
            ).into_response()
        }
        None => {
            if path == "index.html" {
                 return (StatusCode::NOT_FOUND, "Index file not found").into_response();
            }
             // Fallback to index.html for SPA routing if needed, 
             // but here we might just want to return 404 or try index.html
             // Given it's a simple app, let's try to return index.html if it's not found
             // assuming client side routing might be used? 
             // Actually, looking at the previous index.html, it's a single page app but doesn't seem to have complex routing 
             // that requires history API fallback.
             // But for safety, let's just return 404 for assets.
             (StatusCode::NOT_FOUND, "404 Not Found").into_response()
        }
    }
}
