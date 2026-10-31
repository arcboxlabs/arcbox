use std::error::Error;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::io::FromRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use arcbox_vmm::{BlockDeviceConfig, VmBackend, Vmm, VmmConfig};
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer as _;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

const DISK_BYTES: u64 = 256 << 20;
const CMDLINE: &str = "console=hvc0 rdinit=/init loglevel=4 panic=-1";
/// The guest's vsock echo port (`guest/init.c`).
const VSOCK_PORT: u32 = 5000;

/// What the probe needs to rebuild the same machine in another process.
#[derive(serde::Serialize, serde::Deserialize)]
struct Spec {
    kernel: PathBuf,
    initrd: PathBuf,
    vcpus: u32,
    memory: u64,
    /// Kernel parameters appended to [`CMDLINE`] (`SPIKE_APPEND`).
    append: String,
    /// The last tick line the guest printed before the checkpoint.
    last_tick: String,
    /// Host ports of the echo, idle and stream servers the guest's TCP
    /// clients dial (`SPIKE_NET=1`); the machine then has virtio-net and
    /// virtio-vsock.
    #[serde(default)]
    net_ports: Option<[u16; 3]>,
}

pub fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let usage = "usage: checkpoint_linux_spike save|restore <dir> <run_s>";
    let (mode, dir, run_s) = match args.as_slice() {
        [_, m, d, s] => (m.as_str(), PathBuf::from(d), s.parse::<u64>()?),
        _ => return Err(usage.into()),
    };
    let console = Console::install();
    // The network datapath runs on the ambient Tokio runtime.
    let rt = tokio::runtime::Runtime::new()?;
    let _rt = rt.enter();
    match mode {
        "save" => save(&dir, run_s, &console),
        "restore" => restore(&dir, run_s, &console),
        _ => Err(usage.into()),
    }
}

fn config(spec: &Spec, disk: &Path) -> VmmConfig {
    VmmConfig {
        vcpu_count: spec.vcpus,
        memory_size: spec.memory,
        kernel_path: spec.kernel.clone(),
        kernel_cmdline: match spec.net_ports {
            Some([a, b, c]) => format!("{CMDLINE} {} spike_net={a},{b},{c}", spec.append),
            None => format!("{CMDLINE} {}", spec.append),
        },
        initrd_path: Some(spec.initrd.clone()),
        serial_console: true,
        virtio_console: true,
        networking: spec.net_ports.is_some(),
        vsock: spec.net_ports.is_some(),
        guest_cid: spec.net_ports.map(|_| 3),
        balloon: false,
        block_devices: vec![BlockDeviceConfig {
            path: disk.to_path_buf(),
            read_only: false,
        }],
        backend: VmBackend::Hv,
        ..VmmConfig::default()
    }
}

fn save(dir: &Path, run_s: u64, console: &Console) -> Result<()> {
    fs::create_dir_all(dir)?;
    let mut spec = Spec {
        kernel: env_path("SPIKE_KERNEL").map_or_else(default_kernel, Ok)?,
        initrd: env_path("SPIKE_INITRD")
            .unwrap_or_else(|| dir.join("..").join("initramfs.cpio.gz")),
        vcpus: std::env::var("SPIKE_VCPUS").map_or(Ok(4), |v| v.parse())?,
        memory: 1 << 30,
        append: std::env::var("SPIKE_APPEND").unwrap_or_default(),
        last_tick: String::new(),
        net_ports: None,
    };
    let echo = if std::env::var("SPIKE_NET").as_deref() == Ok("1") {
        let servers = Servers::serve([0; 3])?;
        spec.net_ports = Some(servers.ports());
        Some(servers)
    } else {
        None
    };
    let disk = dir.join("live-disk.img");
    fs::File::create(&disk)?.set_len(DISK_BYTES)?;

    let vmm = Arc::new(Mutex::new(Vmm::new(config(&spec, &disk))?));
    let t = Instant::now();
    vmm.lock().unwrap().start()?;
    console.wait_for(|l| l.starts_with("tick "), Duration::from_secs(30))?;
    println!(
        "[save] booted, first tick {:.0} ms after start",
        ms(t.elapsed())
    );
    let driver = echo.as_ref().map(|_| VsockDriver::start(&vmm));
    if driver.is_some() {
        // Checkpoint with every connection open and carrying traffic.
        console.wait_for(
            |l| {
                Tick::parse(l)
                    .and_then(|t| t.net)
                    .is_some_and(|n| n[0] > 0 && n[3] > 0 && n[6] > 0)
            },
            Duration::from_secs(30),
        )?;
        if echo
            .as_ref()
            .is_some_and(|s| s.idle.first_conn.get().is_none())
        {
            return Err("the guest's idle connection did not open".into());
        }
    }
    // Land the pause at an arbitrary point inside a tick period.
    let jitter = Duration::from_micros(u64::from(std::process::id()) * 7919 % 900_000);
    std::thread::sleep(Duration::from_secs(run_s) + jitter);

    let report = vmm.lock().unwrap().hv_checkpoint(dir)?;
    spec.last_tick = console
        .last(|l| l.starts_with("tick "))
        .ok_or("no tick before the checkpoint")?;
    if let Some(d) = &driver {
        d.stop.store(true, Ordering::Release);
    }
    vmm.lock().unwrap().stop()?;
    fs::write(dir.join("spike.json"), serde_json::to_vec_pretty(&spec)?)?;
    fs::remove_file(&disk)?;

    let fails = console
        .lines()
        .iter()
        .filter(|l| l.starts_with("FAIL"))
        .count();
    println!("[save] {report}");
    println!("[save] last tick before checkpoint: {}", spec.last_tick);
    if fails > 0 {
        return Err(format!("guest reported {fails} FAIL lines before the checkpoint").into());
    }
    Ok(())
}

fn restore(dir: &Path, run_s: u64, console: &Console) -> Result<()> {
    let spec: Spec = serde_json::from_slice(&fs::read(dir.join("spike.json"))?)?;
    let saved = Tick::parse(&spec.last_tick).ok_or("bad saved tick")?;
    let run = dir.join(format!("run-{}", std::process::id()));
    fs::create_dir_all(&run)?;
    let disk = run.join("disk0.img");
    if std::env::var("SPIKE_TAMPER").as_deref() == Ok("disk") {
        // Negative control: a disk that does not match guest RAM.
        fs::File::create(&disk)?.set_len(DISK_BYTES)?;
    } else {
        clone(&dir.join("disk0.img"), &disk)?;
    }

    // The guest's TCP client redials the same port.
    let echo = spec.net_ports.map(Servers::serve).transpose()?;
    let vmm = Arc::new(Mutex::new(Vmm::new(config(&spec, &disk))?));
    let t = Instant::now();
    vmm.lock().unwrap().hv_restore(dir)?;
    println!("[restore] hv_restore returned in {:.1} ms", ms(t.elapsed()));
    let driver = echo.as_ref().map(|_| VsockDriver::start(&vmm));
    let first = console.wait_for(|l| l.starts_with("tick "), Duration::from_secs(5));
    let first_ms = ms(t.elapsed());
    std::thread::sleep(Duration::from_secs(run_s));
    if let Some(d) = &driver {
        d.stop.store(true, Ordering::Release);
    }
    vmm.lock().unwrap().stop()?;
    let _ = fs::remove_dir_all(&run);
    let first = first?;

    let lines = console.lines();
    let ticks: Vec<Tick> = lines.iter().filter_map(|l| Tick::parse(l)).collect();
    let last = ticks.last().ok_or("no tick after restore")?;
    let head = Tick::parse(&first).ok_or("bad tick")?;
    println!("[restore] saved  {}", spec.last_tick);
    println!("[restore] first  {first} ({first_ms:.0} ms after hv_restore was called)");
    println!(
        "[restore] last   {}",
        lines.iter().rev().find(|l| l.starts_with("tick ")).unwrap()
    );

    let mut problems = Vec::new();
    for l in lines.iter().filter(|l| l.starts_with("FAIL")).take(5) {
        problems.push(format!("guest: {l}"));
    }
    if head.n != saved.n + 1 {
        problems.push(format!("tick counter jumped {} -> {}", saved.n, head.n));
    }
    // One tick period after the saved tick, give or take scheduling.
    let gap = head.mono_ms.wrapping_sub(saved.mono_ms);
    if !(900..=1500).contains(&gap) {
        problems.push(format!(
            "monotonic clock jumped {gap} ms across the checkpoint"
        ));
    }
    // Every thread completes I/O in every second, from the checkpoint on.
    let mut prev = &saved;
    for t in &ticks {
        for (i, (&before, &after)) in prev.ops.iter().zip(&t.ops).enumerate() {
            if after <= before {
                problems.push(format!(
                    "thread {i} made no progress between ticks {} and {}",
                    prev.n, t.n
                ));
            }
        }
        prev = t;
    }
    if last.fails != 0 {
        problems.push(format!("guest counted {} failures", last.fails));
    }
    if ticks.len() as u64 + 1 < run_s {
        problems.push(format!("only {} ticks in {run_s} s", ticks.len()));
    }
    let done: Vec<u64> = saved
        .ops
        .iter()
        .zip(&last.ops)
        .map(|(b, a)| a.saturating_sub(*b))
        .collect();
    println!(
        "[restore] {} ticks, ops per thread after restore {done:?} (vda, vda, hvc, hvc)",
        ticks.len()
    );
    if let (Some(echo), Some(driver)) = (&echo, &driver) {
        check_net(&saved, last, &lines, t, echo, driver, &mut problems);
    }
    if problems.is_empty() {
        println!("[restore] OK");
        Ok(())
    } else {
        Err(problems.join("; ").into())
    }
}

/// Spike 4: the stale connections of the checkpointed process end in the
/// guest at once, and new ones carry traffic.
fn check_net(
    saved: &Tick,
    last: &Tick,
    lines: &[String],
    t: Instant,
    servers: &Servers,
    driver: &VsockDriver,
    problems: &mut Vec<String>,
) {
    let (Some(before), Some(after)) = (saved.net, last.net) else {
        problems.push("tick lines carry no net counters".into());
        return;
    };
    let d = |i: usize| after[i].saturating_sub(before[i]);
    let since = |at: &OnceLock<Instant>| at.get().map(|a| ms(a.duration_since(t)));
    println!(
        "[restore] tcp: {} echoes, stale flow ended by {} reset / {} timeout, \
         first reconnect {:?} ms; vsock: {} guest / {} host echoes, {} ended, first echo {:?} ms",
        d(0),
        d(1),
        d(2),
        since(&servers.echo.first_conn),
        d(3),
        driver.echoes.load(Ordering::Relaxed),
        d(4),
        since(&driver.first_echo),
    );
    println!(
        "[restore] idle: {} ended, reconnect {:?} ms; download: {} KiB after restore, {} ended, \
         reconnect {:?} ms",
        d(5),
        since(&servers.idle.first_conn),
        d(6),
        d(7),
        since(&servers.stream.first_conn),
    );
    for l in lines.iter().filter(|l| l.contains("-end ")) {
        println!("[restore] guest: {l}");
    }
    // A vsock write after the transport reset reports ENOTCONN (107)
    // rather than ECONNRESET.
    let reset = |l: &&String| {
        l.contains("errno 104") || (l.starts_with("vsock-end") && l.contains("errno 107"))
    };
    if !lines
        .iter()
        .filter(|l| l.contains("-end "))
        .all(|l| reset(&l))
    {
        problems.push("a connection ended other than by a reset".into());
    }
    // The idle client sends nothing, so only a reset from the host ends it.
    if d(5) != 1 || servers.idle.first_conn.get().is_none() {
        problems.push(format!(
            "{} idle connections ended, expected 1 and a reconnect",
            d(5)
        ));
    }
    if d(7) != 1 || d(6) == 0 {
        problems.push(format!(
            "{} downloads ended, {} KiB after restore; expected 1 and progress",
            d(7),
            d(6)
        ));
    }
    if d(0) == 0 {
        problems.push("guest TCP client made no progress".into());
    }
    // The stale flow must end by the datapath's RST, not the client's own
    // receive timeout.
    if (d(1), d(2)) != (1, 0) {
        problems.push(format!(
            "stale TCP flow ended by {} reset / {} timeout, expected one reset",
            d(1),
            d(2)
        ));
    }
    if d(3) == 0 || driver.echoes.load(Ordering::Relaxed) == 0 {
        problems.push("no vsock traffic after restore".into());
    }
    if d(4) != 1 {
        problems.push(format!("{} vsock connections ended, expected 1", d(4)));
    }
}

/// One `tick` line from `guest/init.c`.
struct Tick {
    n: u64,
    mono_ms: u64,
    ops: [u64; 4],
    fails: u64,
    /// TCP echo ok, reset, timeout; vsock ok, ended; idle ended; download
    /// KiB, ended (`spike_net`).
    net: Option<[u64; 8]>,
}

impl Tick {
    fn parse(line: &str) -> Option<Self> {
        let f: Vec<&str> = line.split_whitespace().collect();
        if !matches!(f.len(), 13 | 25) || f[0] != "tick" || f[2] != "mono_ms" || f[6] != "ops" {
            return None;
        }
        let n = |i: usize| f[i].parse::<u64>().ok();
        let net = if f.len() == 25 {
            if (f[13], f[17], f[20], f[22]) != ("net", "vsock", "idle", "down") {
                return None;
            }
            Some([
                n(14)?,
                n(15)?,
                n(16)?,
                n(18)?,
                n(19)?,
                n(21)?,
                n(23)?,
                n(24)?,
            ])
        } else {
            None
        };
        Some(Self {
            n: n(1)?,
            mono_ms: n(3)?,
            ops: [n(7)?, n(8)?, n(9)?, n(10)?],
            fails: n(12)?,
            net,
        })
    }
}

/// The host TCP servers on loopback; the guest reaches them through the
/// gateway address.
struct Servers {
    echo: Server,
    idle: Server,
    stream: Server,
}

impl Servers {
    fn serve([echo, idle, stream]: [u16; 3]) -> Result<Self> {
        Ok(Self {
            echo: Server::serve(echo, Behavior::Echo)?,
            idle: Server::serve(idle, Behavior::Idle)?,
            stream: Server::serve(stream, Behavior::Stream)?,
        })
    }

    fn ports(&self) -> [u16; 3] {
        [self.echo.port, self.idle.port, self.stream.port]
    }
}

#[derive(Clone, Copy)]
enum Behavior {
    /// Echoes what it reads.
    Echo,
    /// Reads until EOF and never writes.
    Idle,
    /// Writes an endless little-endian u64 counter from 0.
    Stream,
}

struct Server {
    port: u16,
    first_conn: Arc<OnceLock<Instant>>,
}

impl Server {
    fn serve(port: u16, behavior: Behavior) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        let port = listener.local_addr()?.port();
        let first_conn = Arc::new(OnceLock::new());
        let first = Arc::clone(&first_conn);
        std::thread::spawn(move || {
            for mut conn in listener.incoming().flatten() {
                let _ = first.set(Instant::now());
                std::thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    match behavior {
                        Behavior::Echo => {
                            while let Ok(n @ 1..) = conn.read(&mut buf) {
                                if conn.write_all(&buf[..n]).is_err() {
                                    break;
                                }
                            }
                        }
                        Behavior::Idle => while let Ok(1..) = conn.read(&mut buf) {},
                        Behavior::Stream => {
                            for word in (0u64..).step_by(buf.len() / 8) {
                                for (i, chunk) in buf.chunks_exact_mut(8).enumerate() {
                                    chunk.copy_from_slice(&(word + i as u64).to_le_bytes());
                                }
                                if conn.write_all(&buf).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        });
        Ok(Self { port, first_conn })
    }
}

/// Keeps one host-to-guest vsock connection busy with 8-byte echoes,
/// reconnecting whenever it ends.
struct VsockDriver {
    stop: Arc<AtomicBool>,
    echoes: Arc<AtomicU64>,
    first_echo: Arc<OnceLock<Instant>>,
}

impl VsockDriver {
    fn start(vmm: &Arc<Mutex<Vmm>>) -> Self {
        let d = Self {
            stop: Arc::default(),
            echoes: Arc::default(),
            first_echo: Arc::new(OnceLock::new()),
        };
        let (vmm, stop, echoes, first) = (
            Arc::clone(vmm),
            Arc::clone(&d.stop),
            Arc::clone(&d.echoes),
            Arc::clone(&d.first_echo),
        );
        std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                let Ok(fd) = vmm.lock().unwrap().connect_vsock(VSOCK_PORT) else {
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                };
                // SAFETY: `connect_vsock` hands over a fresh socket fd.
                let mut s = unsafe { UnixStream::from_raw_fd(fd) };
                let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                for seq in 1u64.. {
                    let mut back = [0u8; 8];
                    if stop.load(Ordering::Acquire)
                        || s.write_all(&seq.to_le_bytes()).is_err()
                        || s.read_exact(&mut back).is_err()
                        || u64::from_le_bytes(back) != seq
                    {
                        break;
                    }
                    let _ = first.set(Instant::now());
                    echoes.fetch_add(1, Ordering::Relaxed);
                    std::thread::sleep(Duration::from_millis(10));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        d
    }
}

/// Collects the guest's hvc0 lines (the `guest_console` tracing target)
/// and echoes them.
#[derive(Clone, Default)]
struct Console(Arc<Mutex<Vec<String>>>);

impl Console {
    fn install() -> Self {
        let console = Self::default();
        tracing_subscriber::registry()
            .with(console.clone())
            .with(
                tracing_subscriber::fmt::layer().with_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "warn".into()),
                ),
            )
            .init();
        console
    }

    fn lines(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }

    fn last(&self, f: impl Fn(&str) -> bool) -> Option<String> {
        self.0.lock().unwrap().iter().rev().find(|l| f(l)).cloned()
    }

    fn wait_for(&self, f: impl Fn(&str) -> bool, timeout: Duration) -> Result<String> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(l) = self.last(&f) {
                return Ok(l);
            }
            if Instant::now() > deadline {
                return Err("timed out waiting for guest output".into());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Console {
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        if event.metadata().target() != "guest_console" {
            return;
        }
        struct Message(String);
        impl Visit for Message {
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}");
                }
            }
        }
        let mut m = Message(String::new());
        event.record(&mut m);
        println!("  guest| {}", m.0);
        self.0.lock().unwrap().push(m.0);
    }
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

/// The kernel of the newest boot-asset version in `~/.arcbox/boot`.
fn default_kernel() -> Result<PathBuf> {
    let boot = env_path("HOME").ok_or("HOME unset")?.join(".arcbox/boot");
    let mut versions: Vec<PathBuf> = fs::read_dir(&boot)?
        .filter_map(|e| e.ok().map(|e| e.path().join("kernel")))
        .filter(|p| p.exists())
        .collect();
    versions.sort();
    versions
        .pop()
        .ok_or_else(|| format!("no kernel under {}", boot.display()).into())
}

/// `clonefile(2)`: an APFS copy-on-write clone.
fn clone(src: &Path, dst: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let s = std::ffi::CString::new(src.as_os_str().as_bytes())?;
    let d = std::ffi::CString::new(dst.as_os_str().as_bytes())?;
    // SAFETY: both are valid NUL-terminated paths.
    if unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), 0) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}
