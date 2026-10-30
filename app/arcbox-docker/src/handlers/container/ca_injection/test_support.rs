//! Shared public-certificate and tar fixtures; no host Keychain access.

use rcgen::{
    BasicConstraints, CertificateParams, CidrSubnet, DistinguishedName, DnType, GeneralSubtree,
    IsCa, KeyPair, KeyUsagePurpose, NameConstraints,
};
use std::io::Read;

pub(super) fn root() -> Vec<u8> {
    root_identity().0.pem().into_bytes()
}

pub(super) fn root_identity() -> (rcgen::Certificate, rcgen::Issuer<'static, KeyPair>) {
    let mut params = CertificateParams::default();
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, "ArcBox Local CA");
    params
        .distinguished_name
        .push(DnType::OrganizationName, "ArcBox");
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.name_constraints = Some(NameConstraints {
        permitted_subtrees: vec![GeneralSubtree::DnsName("arcbox.local".into())],
        excluded_subtrees: vec![
            GeneralSubtree::IpAddress(CidrSubnet::V4([0; 4], [0; 4])),
            GeneralSubtree::IpAddress(CidrSubnet::V6([0; 16], [0; 16])),
        ],
    });
    let key = KeyPair::generate().unwrap();
    let certificate = params.self_signed(&key).unwrap();
    (certificate, rcgen::Issuer::new(params, key))
}

pub(super) fn tar(content: &[u8]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(content.len() as u64);
    header.set_mode(0o640);
    header.set_uid(42);
    header.set_gid(84);
    header.set_mtime(1234);
    builder
        .append_data(&mut header, "ca-certificates.crt", content)
        .unwrap();
    builder.into_inner().unwrap()
}

pub(super) fn untar(bytes: &[u8]) -> (tar::Header, Vec<u8>) {
    let mut archive = tar::Archive::new(bytes);
    let mut entries = archive.entries().unwrap();
    let mut entry = entries.next().unwrap().unwrap();
    let header = entry.header().clone();
    let mut content = Vec::new();
    entry.read_to_end(&mut content).unwrap();
    assert!(entries.next().is_none());
    (header, content)
}
