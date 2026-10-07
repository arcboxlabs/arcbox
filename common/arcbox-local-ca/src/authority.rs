//! Root policy shared by the Keychain authority and certificate tests.

use arcbox_constants::dns::LOCAL_CA_COMMON_NAME;
use rcgen::{
    BasicConstraints, CertificateParams, CidrSubnet, DistinguishedName, DnType, GeneralSubtree,
    IsCa, KeyUsagePurpose, NameConstraints,
};
use time::{Duration, OffsetDateTime};

use crate::LOCAL_DOMAIN;

/// How long a generated CA stays valid.
const LIFETIME: Duration = Duration::days(3650);

pub fn parameters() -> CertificateParams {
    let mut params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, LOCAL_CA_COMMON_NAME);
    name.push(DnType::OrganizationName, "ArcBox");
    params.distinguished_name = name;
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.name_constraints = Some(NameConstraints {
        permitted_subtrees: vec![GeneralSubtree::DnsName(LOCAL_DOMAIN.to_owned())],
        excluded_subtrees: vec![
            GeneralSubtree::IpAddress(CidrSubnet::V4([0; 4], [0; 4])),
            GeneralSubtree::IpAddress(CidrSubnet::V6([0; 16], [0; 16])),
        ],
    });
    let now = OffsetDateTime::now_utc();
    params.not_before = now - Duration::days(1);
    params.not_after = now + LIFETIME;
    params
}
