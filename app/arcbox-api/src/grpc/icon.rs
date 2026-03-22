//! Icon service gRPC implementation.

use arcbox_grpc::IconService;
use arcbox_protocol::v1::{GetImageIconRequest, GetImageIconResponse};
use tonic::{Request, Response, Status};

/// Extracts the serde tag name from an `IconSource` variant.
///
/// `IconSource` is `#[serde(tag = "type", rename_all = "snake_case")]`,
/// so serializing yields `{"type": "<variant_name>", "url": "..."}`.
fn icon_source_name(source: &dimicon::IconSource) -> String {
    serde_json::to_value(source)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str().map(String::from)))
        .unwrap_or_else(|| "unknown".to_string())
}

struct ResolvedIcon(Option<dimicon::IconSource>);

impl From<ResolvedIcon> for GetImageIconResponse {
    fn from(resolved: ResolvedIcon) -> Self {
        match resolved.0 {
            Some(source) => Self {
                source: icon_source_name(&source),
                url: source.url().to_string(),
            },
            None => Self {
                url: String::new(),
                source: "not_found".to_string(),
            },
        }
    }
}

/// Icon service implementation — delegates to `dimicon` for image icon lookups.
pub struct IconServiceImpl {
    icon_service: dimicon::IconService,
}

impl Default for IconServiceImpl {
    fn default() -> Self {
        Self {
            icon_service: dimicon::IconService::new(),
        }
    }
}

impl IconServiceImpl {
    /// Creates a new icon service.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[tonic::async_trait]
impl IconService for IconServiceImpl {
    async fn get_image_icon(
        &self,
        request: Request<GetImageIconRequest>,
    ) -> Result<Response<GetImageIconResponse>, Status> {
        let fqin = request.into_inner().fqin;

        let icon = self
            .icon_service
            .get_icon(&fqin)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(ResolvedIcon(icon).into()))
    }
}
