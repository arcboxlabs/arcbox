//! Replace or remove only the validated ArcBox section of a PEM trust bundle.

mod anchor;
mod file;

pub use anchor::install_trust_anchor;

pub use file::install_trust_bundle;

use crate::{Error, PublicAuthority};

const BEGIN: &str = "\n# BEGIN ARCBOX LOCAL CA\n";
const END: &str = "# END ARCBOX LOCAL CA\n";

/// Replaces the owned root, or removes the owned section when `authority` is `None`.
///
/// Preserves every byte outside the section and retains equivalent existing PEM.
/// Callers must check the incoming root's validity before installing the result.
///
/// # Errors
///
/// Rejects non-text bundles, ambiguous markers, and invalid owned certificates.
pub fn reconcile_trust_bundle(
    bundle: &[u8],
    authority: Option<&PublicAuthority>,
) -> Result<Vec<u8>, Error> {
    let text = std::str::from_utf8(bundle)
        .map_err(|_| Error::InvalidTrustBundle("bundle is not PEM text"))?;
    let starts: Vec<_> = text.match_indices(BEGIN).map(|(index, _)| index).collect();
    let ends: Vec<_> = text.match_indices(END).map(|(index, _)| index).collect();
    let (before, after) = match (starts.as_slice(), ends.as_slice()) {
        ([], []) => (text, ""),
        ([start], [end]) if start + BEGIN.len() <= *end => {
            let previous = PublicAuthority::from_pem(&text.as_bytes()[start + BEGIN.len()..*end])?;
            if authority.is_some_and(|current| previous.der() == current.der()) {
                return Ok(bundle.to_vec());
            }
            (&text[..*start], &text[end + END.len()..])
        }
        _ => return Err(Error::InvalidTrustBundle("ambiguous ArcBox markers")),
    };
    let Some(current) = authority else {
        return Ok([before.as_bytes(), after.as_bytes()].concat());
    };
    let pem = pem::encode_config(
        &pem::Pem::new("CERTIFICATE", current.der()),
        pem::EncodeConfig::new().set_line_ending(pem::LineEnding::LF),
    );
    Ok([
        before.as_bytes(),
        BEGIN.as_bytes(),
        pem.as_bytes(),
        END.as_bytes(),
        after.as_bytes(),
    ]
    .concat())
}

#[cfg(test)]
mod tests {
    use rcgen::{Certificate, CertificateParams, DnType, KeyPair};

    use super::*;

    fn certificate(parameters: CertificateParams) -> Certificate {
        parameters
            .self_signed(&KeyPair::generate().unwrap())
            .unwrap()
    }

    fn root() -> PublicAuthority {
        PublicAuthority::try_from(certificate(crate::authority::parameters()).der().to_vec())
            .unwrap()
    }

    #[test]
    fn rotation_preserves_unrelated_roots_and_text() {
        let mut parameters = crate::authority::parameters();
        parameters
            .distinguished_name
            .push(DnType::CommonName, "User CA");
        let unrelated = certificate(parameters);
        let before = format!("# User prefix\r\n{}\r\n", unrelated.pem());
        let after = b"# User suffix\r\n";
        let old = root();
        let new = root();
        let installed = reconcile_trust_bundle(before.as_bytes(), Some(&old)).unwrap();
        let with_tail = [installed.as_slice(), after].concat();
        let rotated = reconcile_trust_bundle(&with_tail, Some(&new)).unwrap();
        let roots = pem::parse_many(&rotated).unwrap();
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0].contents(), unrelated.der().as_ref());
        assert_eq!(roots[1].contents(), new.der());
        assert_eq!(
            reconcile_trust_bundle(&rotated, Some(&new)).unwrap(),
            rotated
        );
        assert_eq!(
            reconcile_trust_bundle(&rotated, None).unwrap(),
            [before.as_bytes(), after].concat()
        );
    }

    #[test]
    fn removal_retains_unmarked_roots_and_accepts_an_expired_owned_root() {
        let unmarked = certificate(crate::authority::parameters()).pem();
        let mut parameters = crate::authority::parameters();
        parameters.not_after = time::OffsetDateTime::now_utc() - time::Duration::days(1);
        parameters.not_before = parameters.not_after - time::Duration::days(1);
        let expired = PublicAuthority::try_from(certificate(parameters).der().to_vec()).unwrap();
        assert!(
            !expired
                .validity()
                .is_valid_at(time::OffsetDateTime::now_utc())
        );
        let installed = reconcile_trust_bundle(unmarked.as_bytes(), Some(&expired)).unwrap();
        assert_eq!(
            reconcile_trust_bundle(&installed, None).unwrap(),
            unmarked.as_bytes()
        );
        assert_eq!(
            reconcile_trust_bundle(unmarked.as_bytes(), None).unwrap(),
            unmarked.as_bytes()
        );
        assert_eq!(reconcile_trust_bundle(&[], None).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn equivalent_der_preserves_existing_pem_formatting() {
        let root = root();
        let pem = pem::encode(&pem::Pem::new("CERTIFICATE", root.der()));
        let bundle = format!("# prefix{BEGIN}\n{pem}\n{END}# suffix");
        assert_eq!(
            reconcile_trust_bundle(bundle.as_bytes(), Some(&root)).unwrap(),
            bundle.as_bytes()
        );
    }

    #[test]
    fn replacement_and_removal_reject_invalid_or_ambiguous_sections() {
        let root = root();
        let valid = reconcile_trust_bundle(&[], Some(&root)).unwrap();
        let foreign = certificate(CertificateParams::default()).pem();
        let cases = [
            vec![0xff],
            BEGIN.as_bytes().to_vec(),
            END.as_bytes().to_vec(),
            format!("{END}{BEGIN}").into_bytes(),
            format!("{BEGIN}{END}").into_bytes(),
            format!("{BEGIN}invalid certificate\n{END}").into_bytes(),
            format!("{BEGIN}{foreign}{END}").into_bytes(),
            [valid.as_slice(), valid.as_slice()].concat(),
        ];
        for bundle in cases {
            for authority in [None, Some(&root)] {
                assert!(reconcile_trust_bundle(&bundle, authority).is_err());
            }
        }
    }
}
