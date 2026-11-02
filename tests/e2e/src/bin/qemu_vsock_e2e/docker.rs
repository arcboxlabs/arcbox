//! The Docker API on vsock 2375, proxied the way the daemon proxies it: a
//! Unix socket the Docker CLI talks to, each client relayed onto its own
//! vsock connection framed with `HalfCloseStream`.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use arcbox_constants::ports::DOCKER_API_VSOCK_PORT;
use arcbox_transport::vsock::HalfCloseStream;
use arcbox_virtio_vsock::vhost_user::VhostUserVsock;

const IMAGE: &str = "alpine:3.20";
const MIB: usize = 1 << 20;

pub struct Docker {
    socket: PathBuf,
    _proxy: tokio::runtime::Runtime,
}

impl Docker {
    pub fn start(vsock: Arc<VhostUserVsock>, socket: PathBuf) -> Result<Self> {
        let proxy = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()?;
        let _ = std::fs::remove_file(&socket);
        let listener = proxy.block_on(async { tokio::net::UnixListener::bind(&socket) })?;
        proxy.spawn(async move {
            while let Ok((mut client, _)) = listener.accept().await {
                let vsock = Arc::clone(&vsock);
                tokio::spawn(async move {
                    let guest = vsock
                        .connect_vsock(DOCKER_API_VSOCK_PORT)
                        .map(UnixStream::from)
                        .and_then(|s| s.set_nonblocking(true).map(|()| s))
                        .and_then(tokio::net::UnixStream::from_std);
                    match guest {
                        Ok(guest) => {
                            let mut guest = HalfCloseStream::new(guest);
                            let _ = tokio::io::copy_bidirectional(&mut client, &mut guest).await;
                        }
                        Err(e) => tracing::warn!("docker proxy: vsock connect: {e}"),
                    }
                });
            }
        });
        Ok(Self {
            socket,
            _proxy: proxy,
        })
    }

    fn cli(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new("docker");
        cmd.env("DOCKER_HOST", format!("unix://{}", self.socket.display()))
            .args(args);
        cmd
    }

    /// Runs the CLI with `stdin` (closed after writing), failing on a
    /// non-zero exit or once `budget` passes.
    fn run(&self, args: &[&str], stdin: Option<Vec<u8>>, budget: Duration) -> Result<String> {
        let mut child = self
            .cli(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawn docker")?;
        let feeder = stdin.map(|data| {
            let mut pipe = child.stdin.take().expect("piped stdin");
            std::thread::spawn(move || pipe.write_all(&data))
        });
        let out = collect(child.stdout.take().expect("piped"));
        let err = collect(child.stderr.take().expect("piped"));
        let status = wait_bounded(&mut child, budget)
            .with_context(|| format!("docker {}", args.join(" ")))?;
        if let Some(feeder) = feeder {
            feeder.join().expect("feeder")?;
        }
        let (out, err) = (out.join().expect("reader")?, err.join().expect("reader")?);
        let out = String::from_utf8_lossy(&out).into_owned();
        ensure!(
            status.success(),
            "docker {} exited {status}: {}",
            args.join(" "),
            String::from_utf8_lossy(&err)
        );
        Ok(out)
    }

    pub fn wait_ready(&self, budget: Duration) -> Result<String> {
        let deadline = Instant::now() + budget;
        loop {
            match self.run(
                &["version", "--format", "{{.Server.Version}}"],
                None,
                Duration::from_secs(20),
            ) {
                Ok(v) => return Ok(v.trim().to_owned()),
                Err(e) if Instant::now() > deadline => return Err(e),
                Err(_) => std::thread::sleep(Duration::from_secs(2)),
            }
        }
    }

    /// Request/response, streaming and half-close scenarios through the CLI.
    pub fn scenarios(&self) -> Result<()> {
        // The data disk outlives a run; make the pull download.
        let _ = self.run(
            &["rm", "-f", "qv-sleeper", "qv-ticker", "qv-forever"],
            None,
            secs(60),
        );
        let _ = self.run(&["rmi", "-f", IMAGE], None, secs(60));
        let t = Instant::now();
        let out = self.run(&["pull", IMAGE], None, Duration::from_secs(900))?;
        ensure!(
            out.contains("Downloaded newer image"),
            "pull did not download: {out:?}"
        );
        println!(
            "[ok] docker pull {IMAGE} (downloaded): {:.1}s",
            t.elapsed().as_secs_f64()
        );

        let out = self.run(&["run", "--rm", IMAGE, "echo", "hello"], None, secs(120))?;
        ensure!(out.trim() == "hello", "docker run printed {out:?}");
        println!("[ok] docker run --rm echo: guest-side close at container exit");

        let input = b"piped-in".to_vec();
        let out = self.run(
            &["run", "-i", "--rm", IMAGE, "sh", "-c", "cat; echo :end"],
            Some(input),
            secs(120),
        )?;
        ensure!(out == "piped-in:end\n", "docker run -i printed {out:?}");
        println!("[ok] docker run -i (attach): stdin EOF reached the container");

        self.run(
            &["run", "-d", "--name", "qv-sleeper", IMAGE, "sleep", "3600"],
            None,
            secs(120),
        )?;
        let out = self.run(
            &["exec", "-i", "qv-sleeper", "sh", "-c", "wc -c; echo :done"],
            Some(vec![b'x'; MIB]),
            secs(120),
        )?;
        ensure!(
            out.split_whitespace().collect::<Vec<_>>() == ["1048576", ":done"],
            "{out:?}"
        );
        println!("[ok] docker exec -i: 1 MiB in, stdin EOF delivered, output back");

        let ticker = "for i in 1 2 3 4 5; do echo tick$i; sleep 1; done";
        self.run(
            &[
                "run",
                "-d",
                "--name",
                "qv-ticker",
                IMAGE,
                "sh",
                "-c",
                ticker,
            ],
            None,
            secs(120),
        )?;
        let out = self.run(&["logs", "-f", "qv-ticker"], None, secs(120))?;
        ensure!(
            out.lines().collect::<Vec<_>>() == ["tick1", "tick2", "tick3", "tick4", "tick5"],
            "{out:?}"
        );
        println!("[ok] docker logs -f: followed to the end, stream closed by the guest");

        self.logs_follow_host_close()?;
        Ok(())
    }

    /// `docker logs -f` on a container that never stops, killed after the
    /// first lines: the host closes a stream the guest is still writing.
    fn logs_follow_host_close(&self) -> Result<()> {
        let forever = "while true; do echo x; sleep 0.2; done";
        self.run(
            &[
                "run",
                "-d",
                "--name",
                "qv-forever",
                IMAGE,
                "sh",
                "-c",
                forever,
            ],
            None,
            secs(120),
        )?;
        let mut child = self
            .cli(&["logs", "-f", "qv-forever"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let mut stdout = child.stdout.take().expect("piped");
        let first = std::thread::spawn(move || stdout.read_exact(&mut [0u8; 8]));
        let deadline = Instant::now() + secs(60);
        while !first.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        // Killing the CLI also ends a read still waiting for output.
        child.kill()?;
        child.wait()?;
        first
            .join()
            .expect("reader")
            .context("logs -f produced nothing within 60s")?;
        self.run(&["version"], None, secs(30))?;
        println!("[ok] docker logs -f killed mid-stream: host-side close, API still answers");
        Ok(())
    }

    pub fn cleanup(&self) -> Result<()> {
        self.run(
            &["rm", "-f", "qv-sleeper", "qv-ticker", "qv-forever"],
            None,
            secs(120),
        )
        .map(drop)
    }
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn collect(
    mut pipe: impl Read + Send + 'static,
) -> std::thread::JoinHandle<std::io::Result<Vec<u8>>> {
    std::thread::spawn(move || {
        let mut out = Vec::new();
        pipe.read_to_end(&mut out).map(|_| out)
    })
}

fn wait_bounded(
    child: &mut std::process::Child,
    budget: Duration,
) -> Result<std::process::ExitStatus> {
    let deadline = Instant::now() + budget;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("still running after {budget:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
