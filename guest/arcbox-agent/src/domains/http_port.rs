//! Which of a container's ports its domain serves over plain HTTP.

use std::str::FromStr;

/// Container label that pins the HTTP port: a port number, or `off`.
pub const LABEL: &str = "dev.arcbox.http-port";

/// The port a container's domain answers plain HTTP on.
pub const HTTP_PORT: u16 = 80;

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

/// The container's HTTP port, as its label pins it.
#[must_use]
pub fn choose(pin: Option<Pin>) -> Option<u16> {
    match pin {
        Some(Pin::Port(port)) => Some(port),
        Some(Pin::Off) | None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
