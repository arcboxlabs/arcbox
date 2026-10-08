use super::*;

#[test]
fn identity_lookup_rejects_multiple_matches() {
    let certificate = authority::parameters()
        .self_signed(&rcgen::KeyPair::generate().unwrap())
        .unwrap();
    let reference = || {
        SearchResult::Ref(Reference::Certificate(
            SecCertificate::from_der(certificate.der()).unwrap(),
        ))
    };
    assert!(unique_reference(Vec::new()).unwrap().is_none());
    assert!(unique_reference(vec![reference()]).unwrap().is_some());
    assert!(unique_reference(vec![reference(), reference()]).is_err());
}
