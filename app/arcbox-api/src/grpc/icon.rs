//! Icon service gRPC implementation.

use arcbox_grpc::IconService;
use arcbox_protocol::v1::{GetImageIconRequest, GetImageIconResponse};
use tonic::{Request, Response, Status};

struct ResolvedIcon(Option<dimicon::IconSource>);

impl From<ResolvedIcon> for GetImageIconResponse {
    fn from(resolved: ResolvedIcon) -> Self {
        match resolved.0 {
            Some(source) => {
                let name = match &source {
                    dimicon::IconSource::DockerHubLogo { .. } => "docker_hub_logo",
                    dimicon::IconSource::DockerHubOrgGravatar { .. } => "docker_hub_org_gravatar",
                    dimicon::IconSource::DockerOfficialImage { .. } => "docker_official_image",
                    dimicon::IconSource::GhcrAvatar { .. } => "ghcr_avatar",
                    dimicon::IconSource::Devicon { .. } => "devicon",
                    dimicon::IconSource::Custom { .. } => "custom",
                    _ => "unknown",
                };
                Self {
                    url: source.url().to_string(),
                    source: name.to_string(),
                }
            }
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
