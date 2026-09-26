//! Kubernetes LoadBalancer Services reach the host.
//!
//! k3s runs its service load balancer (servicelb) in the System VM, and the
//! daemon forwards every published LoadBalancer port to the Mac through the
//! inbound relay Docker publishes use. One VM, sequential phases: a
//! Service's port answers from the host and is reported forwarded, moves
//! with the Service, closes when the Service is deleted, and every listener
//! is gone by the time a Kubernetes stop returns.
//!
//! Needs registry access from the guest: k3s pulls the klipper-lb image and
//! the workload image (`ARCBOX_E2E_IMAGE`, alpine by default) itself, into
//! its own containerd namespace. Host ports are ephemeral, per the
//! parallel-safety contract in AGENTS.md.

use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use arcbox_e2e::daemon::connect_unix;
use arcbox_e2e::docker::{docker_output_with_input, ensure_image};
use arcbox_e2e::scenario::run_vz_scenario;
use arcbox_grpc::v1::kubernetes_service_client::KubernetesServiceClient;
use arcbox_protocol::v1::kubernetes_host_port::State;
use arcbox_protocol::v1::{KubernetesStartRequest, KubernetesStatusRequest, KubernetesStopRequest};

const READY_TIMEOUT: Duration = Duration::from_secs(180);
/// A cold cluster pulls klipper-lb, CoreDNS and the workload image before
/// servicelb publishes anything, so the first port can take minutes.
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(300);
/// servicelb reacts in seconds and the daemon polls every 2 s.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(60);
const KUBECTL_TIMEOUT: Duration = Duration::from_secs(120);
/// Body the workload serves; distinctive enough that nothing else answers it.
const MARKER: &str = "arcbox-k8s-lb-ok";
const NAME: &str = "arcbox-e2e-lb";

#[test]
#[ignore = "boots a VZ System VM and k3s through a real daemon; run on the e2e runner"]
fn load_balancer_ports_reach_the_host() -> Result<()> {
    if !arcbox_e2e::env_flag("SKIP_BUILD") {
        // The daemon polls the KubernetesLoadBalancers agent RPC, so the
        // guest must run a matching agent; `run_vz_scenario` builds the
        // daemon. Keep in sync with the xtask `kubernetes_lb` prebuild recipe.
        let shell = xshell::Shell::new()?;
        shell.change_dir(arcbox_e2e::repo_root());
        xshell::cmd!(
            shell,
            "cargo build --release -p arcbox-agent --target aarch64-unknown-linux-musl"
        )
        .run()?;
    }

    run_vz_scenario("kubernetes_lb", |daemon, data_dir, metrics| {
        daemon.wait_ready_blocking(READY_TIMEOUT)?;
        let image =
            std::env::var("ARCBOX_E2E_IMAGE").unwrap_or_else(|_| "alpine:latest".to_owned());
        // The guest shell kubectl runs in; k3s pulls its own copy for pods.
        ensure_image(data_dir, &image)?;
        let cluster = Cluster::new(daemon.grpc_socket(), data_dir, &image)?;

        metrics.time("kubernetes_start", || cluster.start())?;
        let (port, moved) = (free_port()?, free_port()?);
        cluster.kubectl(&["apply", "-f", "-"], &manifest(&image, port))?;
        metrics.time("lb_published", || expect_marker(port, PUBLISH_TIMEOUT))?;
        cluster.expect_forwarded(port)?;

        let patch = format!(r#"[{{"op":"replace","path":"/spec/ports/0/port","value":{moved}}}]"#);
        cluster.kubectl(&["patch", "service", NAME, "--type=json", "-p", &patch], "")?;
        metrics.time("lb_moved", || expect_marker(moved, PUBLISH_TIMEOUT))?;
        expect_closed(port, CLOSE_TIMEOUT).context("the old port outlived the move")?;

        cluster.kubectl(&["delete", "service", NAME], "")?;
        metrics.time("lb_deleted", || expect_closed(moved, CLOSE_TIMEOUT))?;

        cluster.kubectl(&["apply", "-f", "-"], &manifest(&image, port))?;
        expect_marker(port, PUBLISH_TIMEOUT)?;
        cluster.stop()?;
        expect_closed(port, Duration::ZERO).context("a listener outlived the stop")
    })
}

/// The cluster in the daemon under test.
struct Cluster<'a> {
    runtime: tokio::runtime::Runtime,
    socket: PathBuf,
    data_dir: &'a Path,
    image: &'a str,
}

impl<'a> Cluster<'a> {
    fn new(socket: PathBuf, data_dir: &'a Path, image: &'a str) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("building a tokio runtime for the Kubernetes RPCs")?;
        Ok(Self {
            runtime,
            socket,
            data_dir,
            image,
        })
    }

    fn client(&self) -> Result<KubernetesServiceClient<tonic::transport::Channel>> {
        let channel = self.runtime.block_on(connect_unix(&self.socket))?;
        Ok(KubernetesServiceClient::new(channel))
    }

    fn start(&self) -> Result<()> {
        let mut client = self.client()?;
        let started = self
            .runtime
            .block_on(client.start(KubernetesStartRequest::default()))?
            .into_inner();
        if !started.api_ready {
            bail!("Kubernetes started without an API: {}", started.detail);
        }
        Ok(())
    }

    fn stop(&self) -> Result<()> {
        let mut client = self.client()?;
        self.runtime
            .block_on(client.stop(KubernetesStopRequest::default()))?;
        Ok(())
    }

    /// The daemon reports `port` forwarded in `abctl kubernetes status`.
    fn expect_forwarded(&self, port: u16) -> Result<()> {
        let mut client = self.client()?;
        let status = self
            .runtime
            .block_on(client.status(KubernetesStatusRequest::default()))?
            .into_inner();
        let reported = status
            .host_ports
            .iter()
            .find(|p| p.name == NAME && p.port == u32::from(port))
            .with_context(|| format!("status lists no port {port}: {:?}", status.host_ports))?;
        if reported.state() != State::Forwarded {
            bail!(
                "port {port} reported {:?}: {}",
                reported.state(),
                reported.detail
            );
        }
        Ok(())
    }

    /// Runs `k3s kubectl` in the guest's root namespaces with `input` on
    /// stdin.
    fn kubectl(&self, args: &[&str], input: &str) -> Result<String> {
        let mut command = vec![
            "run",
            "--rm",
            "-i",
            "--privileged",
            "--pid=host",
            self.image,
            "nsenter",
            "-t",
            "1",
            "-m",
            "-n",
            "-p",
            "--",
            "/run/arcbox/runtime/bin/k3s",
            "kubectl",
            "--kubeconfig=/var/lib/rancher/k3s/k3s.yaml",
        ];
        command.extend_from_slice(args);
        docker_output_with_input(self.data_dir, &command, input.as_bytes(), KUBECTL_TIMEOUT)
            .with_context(|| format!("kubectl {}", args.join(" ")))
    }
}

/// A one-replica HTTP server on busybox `nc` behind a LoadBalancer Service
/// on `port`.
fn manifest(image: &str, port: u16) -> String {
    let len = MARKER.len();
    format!(
        r"apiVersion: apps/v1
kind: Deployment
metadata:
  name: {NAME}
spec:
  replicas: 1
  selector:
    matchLabels:
      app: {NAME}
  template:
    metadata:
      labels:
        app: {NAME}
    spec:
      containers:
        - name: httpd
          image: {image}
          imagePullPolicy: IfNotPresent
          command: ['sh', '-c']
          args:
            - |
              while true; do printf 'HTTP/1.1 200 OK\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{MARKER}' | nc -l -p 8080; done
          ports:
            - containerPort: 8080
---
apiVersion: v1
kind: Service
metadata:
  name: {NAME}
spec:
  type: LoadBalancer
  selector:
    app: {NAME}
  ports:
    - name: http
      port: {port}
      targetPort: 8080
"
    )
}

/// An ephemeral port nothing on the host holds right now.
fn free_port() -> Result<u16> {
    let probe = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    Ok(probe.local_addr()?.port())
}

/// `port` answers from the host with [`MARKER`] within `grace`.
fn expect_marker(port: u16, grace: Duration) -> Result<()> {
    let addr = format!("127.0.0.1:{port}");
    let started = Instant::now();
    loop {
        match arcbox_e2e::http::get(&addr) {
            Ok(body) if body.contains(MARKER) => return Ok(()),
            Ok(body) if started.elapsed() >= grace => {
                bail!("{addr} answered without {MARKER:?}: {body:?}")
            }
            Err(e) if started.elapsed() >= grace => {
                return Err(e).with_context(|| format!("{addr} never answered within {grace:?}"));
            }
            _ => std::thread::sleep(Duration::from_millis(500)),
        }
    }
}

/// Nothing listens on `port` any more, within `grace`.
fn expect_closed(port: u16, grace: Duration) -> Result<()> {
    let started = Instant::now();
    loop {
        match TcpStream::connect((Ipv4Addr::LOCALHOST, port)) {
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => return Ok(()),
            _ if started.elapsed() >= grace => bail!("port {port} still accepts connections"),
            _ => std::thread::sleep(Duration::from_millis(250)),
        }
    }
}
