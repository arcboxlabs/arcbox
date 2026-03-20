use arcbox_api::IconService;
use arcbox_api::grpc::IconServiceImpl;
use arcbox_protocol::v1::GetImageIconRequest;
use tonic::Request;

#[tokio::test]
async fn get_icon_for_official_image() {
    let svc = IconServiceImpl::new();
    let resp = svc
        .get_image_icon(Request::new(GetImageIconRequest {
            reference: "nginx".to_string(),
        }))
        .await
        .unwrap()
        .into_inner();

    assert!(!resp.url.is_empty(), "expected icon URL for nginx");
    assert_eq!(resp.source, "docker_official_image");
}

#[tokio::test]
async fn get_icon_for_dockerhub_org() {
    let svc = IconServiceImpl::new();
    let resp = svc
        .get_image_icon(Request::new(GetImageIconRequest {
            reference: "localstack/localstack".to_string(),
        }))
        .await
        .unwrap()
        .into_inner();

    assert!(!resp.url.is_empty(), "expected icon URL for localstack");
    assert!(
        resp.source == "docker_hub_logo" || resp.source == "docker_hub_org_gravatar",
        "unexpected source: {}",
        resp.source
    );
}

#[tokio::test]
async fn get_icon_for_ghcr() {
    let svc = IconServiceImpl::new();
    let resp = svc
        .get_image_icon(Request::new(GetImageIconRequest {
            reference: "ghcr.io/astral-sh/uv".to_string(),
        }))
        .await
        .unwrap()
        .into_inner();

    assert!(!resp.url.is_empty(), "expected icon URL for ghcr image");
    assert_eq!(resp.source, "ghcr_avatar");
}

#[tokio::test]
async fn get_icon_not_found() {
    let svc = IconServiceImpl::new();
    let resp = svc
        .get_image_icon(Request::new(GetImageIconRequest {
            reference: "registry.example.com/nonexistent/image".to_string(),
        }))
        .await
        .unwrap()
        .into_inner();

    assert!(resp.url.is_empty());
    assert_eq!(resp.source, "not_found");
}
