//! SSH signal names ↔ russh's [`Sig`], whose own conversions are private.
//!
//! Signals travel by name end to end (`"INT"`, as RFC 4254 spells them)
//! because the host and the guest number them differently.

use russh::Sig;

/// The RFC 4254 name of `sig`, without the `SIG` prefix.
pub fn name(sig: &Sig) -> &str {
    match sig {
        Sig::ABRT => "ABRT",
        Sig::ALRM => "ALRM",
        Sig::FPE => "FPE",
        Sig::HUP => "HUP",
        Sig::ILL => "ILL",
        Sig::INT => "INT",
        Sig::KILL => "KILL",
        Sig::PIPE => "PIPE",
        Sig::QUIT => "QUIT",
        Sig::SEGV => "SEGV",
        Sig::TERM => "TERM",
        Sig::USR1 => "USR1",
        Sig::Custom(name) => name,
    }
}

/// The [`Sig`] for a signal name; names russh has no variant for (`USR2`)
/// travel as [`Sig::Custom`], which puts them on the wire verbatim.
pub fn from_name(name: &str) -> Sig {
    match name {
        "ABRT" => Sig::ABRT,
        "ALRM" => Sig::ALRM,
        "FPE" => Sig::FPE,
        "HUP" => Sig::HUP,
        "ILL" => Sig::ILL,
        "INT" => Sig::INT,
        "KILL" => Sig::KILL,
        "PIPE" => Sig::PIPE,
        "QUIT" => Sig::QUIT,
        "SEGV" => Sig::SEGV,
        "TERM" => Sig::TERM,
        "USR1" => Sig::USR1,
        other => Sig::Custom(other.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_including_ones_russh_lacks() {
        for signal in ["INT", "KILL", "TERM", "USR1", "USR2", "WINCH"] {
            assert_eq!(name(&from_name(signal)), signal);
        }
        assert!(matches!(from_name("TERM"), Sig::TERM));
        assert!(matches!(from_name("USR2"), Sig::Custom(name) if name == "USR2"));
    }
}
