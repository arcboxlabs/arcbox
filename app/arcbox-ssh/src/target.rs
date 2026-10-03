//! Where a login lands, spelled in the SSH user name: `machine` or
//! `user@machine` (so `ssh root@ubuntu@arcbox` logs into machine `ubuntu`
//! as `root`).

use std::fmt;
use std::str::FromStr;

/// A machine and the account inside it that an SSH login selects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The machine.
    pub machine: String,
    /// The account inside it; `None` leaves the choice to the machine
    /// (root).
    pub user: Option<String>,
}

/// An SSH user name that names no machine.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("'{0}' is not <machine> or <user>@<machine>")]
pub struct InvalidTarget(String);

impl FromStr for Target {
    type Err = InvalidTarget;

    fn from_str(login: &str) -> Result<Self, Self::Err> {
        let invalid = || InvalidTarget(login.to_owned());
        // Split at the last `@`: OpenSSH already took `…@arcbox` off, and a
        // machine name never carries one.
        let (user, machine) = match login.rsplit_once('@') {
            Some((user, machine)) => (Some(user), machine),
            None => (None, login),
        };
        if machine.is_empty() || user.is_some_and(str::is_empty) {
            return Err(invalid());
        }
        Ok(Self {
            machine: machine.to_owned(),
            user: user.map(str::to_owned),
        })
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.user {
            Some(user) => write!(f, "{user}@{}", self.machine),
            None => f.write_str(&self.machine),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_name_is_the_machine_with_its_default_user() {
        let target: Target = "ubuntu".parse().unwrap();
        assert_eq!(target.machine, "ubuntu");
        assert_eq!(target.user, None);
    }

    #[test]
    fn user_at_machine_selects_the_account() {
        let target: Target = "dev@ubuntu".parse().unwrap();
        assert_eq!(target.machine, "ubuntu");
        assert_eq!(target.user.as_deref(), Some("dev"));
        assert_eq!(target.to_string(), "dev@ubuntu");
    }

    #[test]
    fn empty_parts_are_rejected() {
        for login in ["", "@ubuntu", "dev@"] {
            assert!(login.parse::<Target>().is_err(), "{login:?}");
        }
    }
}
