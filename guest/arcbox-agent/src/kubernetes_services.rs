//! The cluster's LoadBalancer Services, reduced to what the host forwards.
//!
//! The agent lists Services with `kubectl get --raw /api/v1/services` — the
//! API server's own `ServiceList`, so no discovery or printer runs — and this
//! module keeps the Services of type LoadBalancer with their ports and
//! ingress addresses. Pure, so its tests run on a host build too.

use arcbox_connect::v1::{KubernetesLoadBalancer, KubernetesServicePort};
use serde::Deserialize;

/// The slice of a `ServiceList` the host needs. The API server defaults
/// `spec.type` and every port's `protocol`, so both are always present;
/// `spec.ports` is absent only for `ExternalName` Services.
#[derive(Deserialize)]
struct ServiceList {
    items: Vec<Service>,
}

#[derive(Deserialize)]
struct Service {
    metadata: Metadata,
    spec: Spec,
    #[serde(default)]
    status: Status,
}

#[derive(Deserialize)]
struct Metadata {
    namespace: String,
    name: String,
}

#[derive(Deserialize)]
struct Spec {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    ports: Vec<Port>,
}

#[derive(Deserialize)]
struct Port {
    protocol: String,
    port: u16,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Status {
    #[serde(default)]
    load_balancer: LoadBalancerStatus,
}

/// `status.loadBalancer`, which the API server renders as `{}` until a load
/// balancer implementation fills it.
#[derive(Deserialize, Default)]
struct LoadBalancerStatus {
    #[serde(default)]
    ingress: Vec<Ingress>,
}

#[derive(Deserialize)]
struct Ingress {
    ip: Option<String>,
    hostname: Option<String>,
}

/// The Services of type LoadBalancer in a `ServiceList`, ordered by
/// namespace and name.
pub fn load_balancers(service_list: &[u8]) -> serde_json::Result<Vec<KubernetesLoadBalancer>> {
    let list: ServiceList = serde_json::from_slice(service_list)?;
    let mut load_balancers: Vec<_> = list
        .items
        .into_iter()
        .filter(|service| service.spec.kind == "LoadBalancer")
        .map(|service| KubernetesLoadBalancer {
            namespace: service.metadata.namespace,
            name: service.metadata.name,
            ports: service
                .spec
                .ports
                .into_iter()
                .map(|port| KubernetesServicePort {
                    protocol: port.protocol,
                    port: u32::from(port.port),
                    ..Default::default()
                })
                .collect(),
            ingress: service
                .status
                .load_balancer
                .ingress
                .into_iter()
                .filter_map(|ingress| ingress.ip.or(ingress.hostname))
                .collect(),
            ..Default::default()
        })
        .collect();
    load_balancers.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    Ok(load_balancers)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from a real k3s v1.36 `GET /api/v1/services`.
    const SERVICE_LIST: &str = r#"{
      "kind": "ServiceList",
      "apiVersion": "v1",
      "metadata": {"resourceVersion": "596"},
      "items": [
        {
          "metadata": {"name": "web", "namespace": "default"},
          "spec": {
            "type": "LoadBalancer",
            "ports": [
              {"name": "http", "protocol": "TCP", "port": 18080, "targetPort": 80, "nodePort": 30455},
              {"name": "dns", "protocol": "UDP", "port": 53, "targetPort": 53, "nodePort": 31053}
            ]
          },
          "status": {"loadBalancer": {"ingress": [{"ip": "10.0.2.2", "ipMode": "VIP"}]}}
        },
        {
          "metadata": {"name": "kubernetes", "namespace": "default"},
          "spec": {"type": "ClusterIP", "ports": [{"protocol": "TCP", "port": 443, "targetPort": 6443}]},
          "status": {"loadBalancer": {}}
        },
        {
          "metadata": {"name": "api", "namespace": "apps"},
          "spec": {"type": "LoadBalancer", "ports": [{"protocol": "TCP", "port": 80, "targetPort": 8080}]},
          "status": {"loadBalancer": {}}
        },
        {
          "metadata": {"name": "external", "namespace": "apps"},
          "spec": {"type": "ExternalName", "externalName": "example.com"},
          "status": {"loadBalancer": {}}
        }
      ]
    }"#;

    fn port(protocol: &str, port: u32) -> KubernetesServicePort {
        KubernetesServicePort {
            protocol: protocol.to_owned(),
            port,
            ..Default::default()
        }
    }

    #[test]
    fn keeps_only_load_balancers_with_their_ports_and_ingress() {
        let got = load_balancers(SERVICE_LIST.as_bytes()).unwrap();

        assert_eq!(
            got,
            vec![
                KubernetesLoadBalancer {
                    namespace: "apps".into(),
                    name: "api".into(),
                    ports: vec![port("TCP", 80)],
                    ingress: Vec::new(),
                    ..Default::default()
                },
                KubernetesLoadBalancer {
                    namespace: "default".into(),
                    name: "web".into(),
                    ports: vec![port("TCP", 18080), port("UDP", 53)],
                    ingress: vec!["10.0.2.2".into()],
                    ..Default::default()
                },
            ]
        );
    }

    #[test]
    fn a_hostname_ingress_counts_as_published() {
        let list = r#"{"items": [{
          "metadata": {"name": "web", "namespace": "default"},
          "spec": {"type": "LoadBalancer", "ports": [{"protocol": "TCP", "port": 80}]},
          "status": {"loadBalancer": {"ingress": [{"hostname": "lb.example"}]}}
        }]}"#;

        let got = load_balancers(list.as_bytes()).unwrap();

        assert_eq!(got[0].ingress, vec!["lb.example".to_owned()]);
    }
}
