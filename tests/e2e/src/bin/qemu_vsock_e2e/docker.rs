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

    /// Bulk transfer through `docker exec`, both directions, and many
    /// host→guest streams at once.
    pub fn throughput(
        &self,
        qemu_pid: u32,
        bytes: usize,
        streams: usize,
        per_stream: usize,
    ) -> Result<()> {
        let cpu = Cpu::start(qemu_pid)?;
        let t = Instant::now();
        let out = self.run(
            &["exec", "-i", "qv-sleeper", "wc", "-c"],
            Some(vec![0u8; bytes]),
            secs(600),
        )?;
        ensure!(out.trim() == bytes.to_string(), "wc -c printed {out:?}");
        println!(
            "[ok] host→guest {} MiB via docker exec -i: {}; {}",
            bytes / MIB,
            rate(bytes, t.elapsed()),
            cpu.lap(t.elapsed())?
        );

        let t = Instant::now();
        let count = bytes.to_string();
        let out = self
            .cli(&["exec", "qv-sleeper", "head", "-c", &count, "/dev/zero"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let received = drain_count(out, secs(600))?;
        ensure!(received == bytes, "received {received} of {bytes} bytes");
        println!(
            "[ok] guest→host {} MiB via docker exec: {}; {}",
            bytes / MIB,
            rate(bytes, t.elapsed()),
            cpu.lap(t.elapsed())?
        );

        let t = Instant::now();
        let results: Vec<Result<String>> = std::thread::scope(|s| {
            let threads: Vec<_> = (0..streams)
                .map(|_| {
                    s.spawn(|| {
                        self.run(
                            &["exec", "-i", "qv-sleeper", "wc", "-c"],
                            Some(vec![0u8; per_stream]),
                            secs(600),
                        )
                    })
                })
                .collect();
            threads
                .into_iter()
                .map(|t| t.join().expect("thread"))
                .collect()
        });
        for r in results {
            ensure!(
                r?.trim() == per_stream.to_string(),
                "a concurrent stream lost bytes"
            );
        }
        println!(
            "[ok] {streams} concurrent host→guest streams of {} MiB, all bytes delivered: {}; {}",
            per_stream / MIB,
            rate(streams * per_stream, t.elapsed()),
            cpu.lap(t.elapsed())?
        );
        Ok(())
    }

    /// `GET /_ping` on a fresh vsock connection each: connect, framed HTTP
    /// round trip through the agent's proxy to dockerd, close.
    pub fn ping_latency(&self, rounds: usize) -> Result<()> {
        let mut samples = Vec::with_capacity(rounds);
        for _ in 0..rounds {
            let t = Instant::now();
            let mut s = UnixStream::connect(&self.socket)?;
            s.set_read_timeout(Some(secs(30)))?;
            s.set_write_timeout(Some(secs(30)))?;
            s.write_all(b"GET /_ping HTTP/1.1\r\nHost: docker\r\nConnection: close\r\n\r\n")?;
            let mut reply = String::new();
            s.read_to_string(&mut reply)?;
            ensure!(
                reply.starts_with("HTTP/1.1 200") && reply.ends_with("OK"),
                "{reply:?}"
            );
            samples.push(t.elapsed());
        }
        println!(
            "[ok] {rounds} Docker GET /_ping, fresh connection each: {}",
            crate::percentiles(&mut samples)
        );
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

/// CPU time of this process (the vsock back-end and the Docker proxy) and
/// of QEMU (the emulated guest), between laps.
struct Cpu {
    qemu_pid: u32,
    last: std::cell::Cell<(f64, f64)>,
}

impl Cpu {
    fn start(qemu_pid: u32) -> Result<Self> {
        let now = (own_cpu(), process_cpu(qemu_pid)?);
        Ok(Self {
            qemu_pid,
            last: std::cell::Cell::new(now),
        })
    }

    fn lap(&self, wall: Duration) -> Result<String> {
        let now = (own_cpu(), process_cpu(self.qemu_pid)?);
        let (own, qemu) = (now.0 - self.last.get().0, now.1 - self.last.get().1);
        self.last.set(now);
        let wall = wall.as_secs_f64();
        Ok(format!(
            "CPU: back-end+proxy {:.0}%, QEMU {:.0}%",
            own / wall * 100.0,
            qemu / wall * 100.0
        ))
    }
}

fn own_cpu() -> f64 {
    // SAFETY: getrusage writes one `rusage`.
    let usage = unsafe {
        let mut usage = std::mem::zeroed::<libc::rusage>();
        libc::getrusage(libc::RUSAGE_SELF, &raw mut usage);
        usage
    };
    let secs = |t: libc::timeval| t.tv_sec as f64 + f64::from(t.tv_usec) / 1e6;
    secs(usage.ru_utime) + secs(usage.ru_stime)
}

/// `ps -o cputime` of `pid`, `[[dd-]hh:]mm:ss.cc`, in seconds.
fn process_cpu(pid: u32) -> Result<f64> {
    let out = Command::new("ps")
        .args(["-o", "cputime=", "-p", &pid.to_string()])
        .output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut secs = 0.0;
    for part in text.trim().split(':') {
        secs = secs * 60.0
            + part
                .parse::<f64>()
                .with_context(|| format!("ps cputime {text:?}"))?;
    }
    Ok(secs)
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn rate(bytes: usize, elapsed: Duration) -> String {
    format!(
        "{:.1} MiB/s ({:.2}s)",
        bytes as f64 / MIB as f64 / elapsed.as_secs_f64(),
        elapsed.as_secs_f64()
    )
}

fn collect(
    mut pipe: impl Read + Send + 'static,
) -> std::thread::JoinHandle<std::io::Result<Vec<u8>>> {
    std::thread::spawn(move || {
        let mut out = Vec::new();
        pipe.read_to_end(&mut out).map(|_| out)
    })
}

/// Counts the child's stdout until it exits, killing it after `budget`.
fn drain_count(mut child: std::process::Child, budget: Duration) -> Result<usize> {
    let mut stdout = child.stdout.take().expect("piped");
    let counter = std::thread::spawn(move || -> std::io::Result<usize> {
        let mut buf = vec![0u8; MIB];
        let mut total = 0;
        loop {
            match stdout.read(&mut buf)? {
                0 => return Ok(total),
                n => total += n,
            }
        }
    });
    let status = wait_bounded(&mut child, budget).context("docker exec")?;
    let total = counter.join().expect("counter")?;
    ensure!(status.success(), "docker exec exited {status}");
    Ok(total)
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
