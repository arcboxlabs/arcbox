//! Validate machine web policies at the Connect boundary.

use arcbox_connect::v1::{self as pb, web_port_policy::Selection};

use super::{EngineError, MachineWebPorts, Result, WebPortPolicy};

impl TryFrom<&pb::WebPortPolicy> for WebPortPolicy {
    type Error = EngineError;

    fn try_from(value: &pb::WebPortPolicy) -> Result<Self> {
        match &value.selection {
            Some(Selection::Auto(_)) => Ok(Self::Auto),
            Some(Selection::Off(_)) => Ok(Self::Off),
            Some(Selection::Port(port)) => u16::try_from(*port)
                .ok()
                .and_then(std::num::NonZeroU16::new)
                .map(Self::Port)
                .ok_or_else(|| EngineError::config("web port must be from 1 through 65535")),
            None => Err(EngineError::config("web port selection is required")),
        }
    }
}

impl From<WebPortPolicy> for pb::WebPortPolicy {
    fn from(value: WebPortPolicy) -> Self {
        Self {
            selection: Some(match value {
                WebPortPolicy::Auto => Selection::Auto(Box::default()),
                WebPortPolicy::Off => Selection::Off(Box::default()),
                WebPortPolicy::Port(port) => Selection::Port(u32::from(port.get())),
            }),
            ..Default::default()
        }
    }
}

impl From<MachineWebPorts> for pb::MachineWebPorts {
    fn from(value: MachineWebPorts) -> Self {
        Self {
            http_port: pb::WebPortPolicy::from(value.http_port).into(),
            https_port: pb::WebPortPolicy::from(value.https_port).into(),
            ..Default::default()
        }
    }
}

impl TryFrom<&pb::MachineWebPorts> for MachineWebPorts {
    type Error = EngineError;

    fn try_from(value: &pb::MachineWebPorts) -> Result<Self> {
        Ok(Self {
            http_port: WebPortPolicy::try_from(&*value.http_port)?,
            https_port: WebPortPolicy::try_from(&*value.https_port)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buffa::Message as _;

    #[test]
    fn rejects_invalid_ports_and_missing_or_unknown_selections() {
        for port in [0, 65536, u32::MAX] {
            let policy = pb::WebPortPolicy {
                selection: Some(Selection::Port(port)),
                ..Default::default()
            };
            assert!(WebPortPolicy::try_from(&policy).is_err());
        }
        for mut bytes in [&[][..], &[0x20, 1][..]] {
            let policy = pb::WebPortPolicy::decode(&mut bytes).unwrap();
            assert!(WebPortPolicy::try_from(&policy).is_err());
        }
    }
}
