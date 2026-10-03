//! Which of a container's ports its domain serves over plain HTTP.

use std::collections::BTreeSet;
use std::str::FromStr;

/// Container label that pins the HTTP port: a port number, or `off`.
pub const LABEL: &str = "dev.arcbox.http-port";

/// The port a container's domain answers plain HTTP on.
pub const HTTP_PORT: u16 = 80;

/// The port a container's domain answers HTTPS on.
pub const HTTPS_PORT: u16 = 443;

/// What [`LABEL`] asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pin {
    /// The container's domain serves no HTTP port.
    Off,
    /// The container's domain serves this port.
    Port(u16),
}

impl FromStr for Pin {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        if value.eq_ignore_ascii_case("off") {
            return Ok(Self::Off);
        }
        match value.parse::<u16>() {
            Ok(port) if port != 0 => Ok(Self::Port(port)),
            _ => Err(format!(
                "{LABEL}={value:?} is neither a port number nor \"off\""
            )),
        }
    }
}

/// The container's HTTP port, given the label, the ports it listens on, and
/// the ports it exposes (`EXPOSE`, `--expose`, `-p`). In order:
///
/// 1. the label, when set (`off`: none);
/// 2. [`HTTP_PORT`], when the container listens there itself;
/// 3. the lowest listening port the container exposes;
/// 4. the lowest listening port.
///
/// [`HTTPS_PORT`] never counts: whatever listens there speaks TLS, and the
/// container keeps it.
#[must_use]
pub fn choose(pin: Option<Pin>, listening: &BTreeSet<u16>, exposed: &BTreeSet<u16>) -> Option<u16> {
    match pin {
        Some(Pin::Off) => None,
        Some(Pin::Port(port)) => Some(port),
        None if listening.contains(&HTTP_PORT) => Some(HTTP_PORT),
        None => {
            let mut candidates = listening.iter().copied().filter(|&p| p != HTTPS_PORT);
            candidates
                .clone()
                .find(|port| exposed.contains(port))
                .or_else(|| candidates.next())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ports(list: &[u16]) -> BTreeSet<u16> {
        list.iter().copied().collect()
    }

    #[test]
    fn the_label_wins_over_what_the_container_listens_on() {
        let listening = ports(&[80, 3000]);
        assert_eq!(choose(Some(Pin::Off), &listening, &ports(&[])), None);
        assert_eq!(
            choose(Some(Pin::Port(8080)), &listening, &ports(&[])),
            Some(8080)
        );
    }

    #[test]
    fn a_container_listening_on_80_keeps_it() {
        assert_eq!(choose(None, &ports(&[3000, 80]), &ports(&[3000])), Some(80));
    }

    #[test]
    fn an_exposed_listener_beats_a_lower_unexposed_one() {
        assert_eq!(
            choose(None, &ports(&[2112, 8080, 9000]), &ports(&[8080, 9000])),
            Some(8080)
        );
    }

    #[test]
    fn without_exposed_listeners_the_lowest_port_serves() {
        assert_eq!(choose(None, &ports(&[9229, 3000]), &ports(&[])), Some(3000));
        assert_eq!(
            choose(None, &ports(&[9229, 3000]), &ports(&[5000])),
            Some(3000),
            "an exposed port nothing listens on does not count"
        );
    }

    #[test]
    fn a_tls_listener_on_443_is_never_the_http_port() {
        assert_eq!(choose(None, &ports(&[443]), &ports(&[443])), None);
        assert_eq!(
            choose(None, &ports(&[443, 8080]), &ports(&[443])),
            Some(8080)
        );
        assert_eq!(choose(None, &ports(&[]), &ports(&[3000])), None);
    }

    #[test]
    fn label_values_parse_strictly() {
        assert_eq!("off".parse(), Ok(Pin::Off));
        assert_eq!(" OFF ".parse(), Ok(Pin::Off));
        assert_eq!("3000".parse(), Ok(Pin::Port(3000)));
        assert!("0".parse::<Pin>().is_err());
        assert!("65536".parse::<Pin>().is_err());
        assert!("http".parse::<Pin>().is_err());
        assert!("".parse::<Pin>().is_err());
    }
}
