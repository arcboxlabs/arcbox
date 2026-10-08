//! Saved machine web backend selection. Routing consumes these settings separately.

use std::fmt;
use std::num::NonZeroU16;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use super::{MachineManager, MachineState};
use crate::error::{EngineError, Result};

#[cfg(test)]
mod tests;

/// Selection for one machine web protocol.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum WebPortPolicy {
    /// Discover a backend automatically.
    #[default]
    Auto,
    /// Disable the protocol.
    Off,
    /// Use this backend port; HTTPS expects native TLS on this port.
    Port(NonZeroU16),
}

impl FromStr for WebPortPolicy {
    type Err = EngineError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "auto" => Ok(Self::Auto),
            "off" => Ok(Self::Off),
            port => port.parse().map(Self::Port).map_err(|_| {
                EngineError::config("web port must be auto, off, or a port from 1 through 65535")
            }),
        }
    }
}

impl fmt::Display for WebPortPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => formatter.write_str("auto"),
            Self::Off => formatter.write_str("off"),
            Self::Port(port) => port.fmt(formatter),
        }
    }
}

impl TryFrom<String> for WebPortPolicy {
    type Error = EngineError;

    fn try_from(value: String) -> Result<Self> {
        value.parse()
    }
}

impl From<WebPortPolicy> for String {
    fn from(value: WebPortPolicy) -> Self {
        value.to_string()
    }
}

/// Saved backend selection, inherited by machine clones and archives.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MachineWebPorts {
    /// HTTP backend selection.
    pub http_port: WebPortPolicy,
    /// HTTPS backend selection; an explicit port uses native TLS.
    pub https_port: WebPortPolicy,
}

impl MachineManager {
    /// Saves backend selection without applying routes. Omitted fields retain their values.
    ///
    /// # Errors
    /// Returns an error for an unknown or transitioning machine, unavailable storage, or a failed save.
    pub fn set_web_ports(
        &self,
        name: &str,
        http_port: Option<WebPortPolicy>,
        https_port: Option<WebPortPolicy>,
    ) -> Result<MachineWebPorts> {
        let _storage = self.storage_permit(name)?;
        let mut machines = self
            .machines
            .write()
            .map_err(|_| EngineError::LockPoisoned)?;
        let machine = machines
            .get_mut(name)
            .ok_or_else(|| EngineError::not_found(name))?;
        if matches!(
            machine.state,
            MachineState::Starting | MachineState::Stopping
        ) {
            return Err(EngineError::invalid_state(format!(
                "cannot configure web ports for machine '{name}' while it is {:?}",
                machine.state
            )));
        }
        let web_ports = MachineWebPorts {
            http_port: http_port.unwrap_or(machine.web_ports.http_port),
            https_port: https_port.unwrap_or(machine.web_ports.https_port),
        };
        if web_ports != machine.web_ports {
            self.persistence
                .update(name, |saved| saved.web_ports = web_ports)?;
            machine.web_ports = web_ports;
        }
        Ok(web_ports)
    }
}
