crate::handlers::proxy_handler!(list_images);
/// Forwards image-load uploads to guest dockerd through the upload-specific
/// proxy path.
pub async fn load_image(
    axum::extract::State(state): axum::extract::State<crate::api::AppState>,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    req: axum::http::Request<axum::body::Body>,
) -> crate::error::Result<axum::response::Response> {
    crate::handlers::proxy_upload(&state, &uri, req).await
}
crate::handlers::proxy_handler!(pull_image);
crate::handlers::proxy_handler!(inspect_image);
crate::handlers::proxy_handler!(remove_image);
crate::handlers::proxy_handler!(tag_image);
crate::handlers::proxy_handler!(get_image);
crate::handlers::proxy_handler!(get_images);
