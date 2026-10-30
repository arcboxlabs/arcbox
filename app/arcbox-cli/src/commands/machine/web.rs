//! Read or save machine web policies without applying routes.

use anyhow::Result;
use arcbox_connect::v1 as pb;
use arcbox_engine::machine::{MachineWebPorts, WebPortPolicy};
use clap::Args;

#[derive(Args)]
pub struct WebArgs {
    /// Machine name
    pub name: String,
    /// HTTP backend selection; omission preserves the saved setting
    #[arg(long, value_name = "auto|off|PORT")]
    pub http_port: Option<WebPortPolicy>,
    /// HTTPS backend selection; an explicit port uses native TLS
    #[arg(long, value_name = "auto|off|PORT")]
    pub https_port: Option<WebPortPolicy>,
}

impl From<&WebArgs> for pb::SetMachineWebPortsRequest {
    fn from(args: &WebArgs) -> Self {
        Self {
            id: args.name.clone(),
            http_port: args.http_port.map(pb::WebPortPolicy::from).into(),
            https_port: args.https_port.map(pb::WebPortPolicy::from).into(),
            ..Default::default()
        }
    }
}

pub async fn execute(args: WebArgs) -> Result<()> {
    let client = super::machine_client();
    let saved = if args.http_port.is_none() && args.https_port.is_none() {
        let machine: pb::MachineInfo = client
            .inspect(pb::InspectMachineRequest {
                id: args.name.clone(),
                ..Default::default()
            })
            .await
            .map_err(|error| crate::error::machine_operation(error, &args.name, "inspect"))?
            .into_owned();
        MachineWebPorts::try_from(&*machine.web_ports)?
    } else {
        let response: pb::MachineWebPorts = client
            .set_web_ports(pb::SetMachineWebPortsRequest::from(&args))
            .await
            .map_err(|error| {
                crate::error::machine_lifecycle(error, &args.name, "configure web ports for")
            })?
            .into_owned();
        MachineWebPorts::try_from(&response)?
    };
    println!("Saved web configuration for '{}':", args.name);
    println!("HTTP:  {}", saved.http_port);
    println!("HTTPS: {}", saved.https_port);
    println!("Configuration only; machine web routing is not yet available.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Command {
        #[command(flatten)]
        args: WebArgs,
    }

    #[test]
    fn options_preserve_omission_in_the_request() {
        let args = Command::try_parse_from(["web", "dev"]).unwrap().args;
        assert!(args.http_port.is_none() && args.https_port.is_none());
        for (flag, value) in [
            ("--http-port", "8080"),
            ("--https-port", "off"),
            ("--https-port", "auto"),
        ] {
            let args = Command::try_parse_from(["web", "dev", flag, value])
                .unwrap()
                .args;
            let request = pb::SetMachineWebPortsRequest::from(&args);
            let (present, omitted) = if flag == "--http-port" {
                (&request.http_port, &request.https_port)
            } else {
                (&request.https_port, &request.http_port)
            };
            assert!(!omitted.is_set());
            assert_eq!(
                WebPortPolicy::try_from(present.as_option().unwrap()).unwrap(),
                value.parse().unwrap()
            );
        }
    }

    #[test]
    fn invalid_ports_are_rejected_before_a_request() {
        for value in ["0", "65536", "https"] {
            assert!(Command::try_parse_from(["web", "dev", "--http-port", value]).is_err());
        }
    }
}
