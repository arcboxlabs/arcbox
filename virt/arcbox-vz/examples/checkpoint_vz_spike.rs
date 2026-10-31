//! VZ checkpoint spike: save a Linux guest's state in the middle of block
//! I/O with Virtualization.framework, and restore it in another process.
//!
//! ```text
//! checkpoint_vz_spike validate                 # which devices allow save/restore
//! checkpoint_vz_spike save    <dir> <run_s>    # boot, run, save into <dir>
//! checkpoint_vz_spike restore <dir> <run_s>    # restore <dir>, run, verify
//! ```
//!
//! The guest is the HV spike 3 initramfs (`arcbox-vmm/examples/
//! checkpoint_linux_spike/guest`): four O_DIRECT threads verify every
//! read on `/dev/vda` against the seq numbers they keep in RAM, and a tick
//! line a second goes to hvc0. `save` pauses the VM, saves its state, clones
//! the disk and stops; `restore` builds the same configuration with the
//! saved machine identifier, restores, resumes, and passes when ticks
//! continue from the saved one, every thread keeps completing I/O, and no
//! read-back check fails.
//!
//! Environment: `SPIKE_KERNEL` (default `~/.arcbox/boot/<newest>/kernel`),
//! `SPIKE_INITRD` (default `<dir>/../initramfs.cpio.gz`), `SPIKE_VCPUS`
//! (save, default 4). Negative controls (restore): `SPIKE_TAMPER=disk`
//! (empty disk), `machine-id` (a new identifier), `memory` (512 MiB more
//! RAM), `state` (one byte of the state file flipped). The binary needs the
//! virtualization entitlement:
//! `codesign --force --entitlements bundle/arcbox.dev.entitlements -s - <bin>`.
//! `save` fails with "permission denied" while the screen is locked: VZ
//! cannot create the Secure Enclave key that protects the state file.
//!
//! Method and results: `docs/experiments/2026-10-31-vz-checkpoint-spike.md`.

use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::io::{FromRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arcbox_vz::{
    EntropyDeviceConfiguration, GenericPlatform, LinuxBootLoader, LinuxRosettaDirectoryShare,
    MemoryBalloonDeviceConfiguration, NetworkDeviceConfiguration, SerialPortConfiguration,
    SharedDirectory, SingleDirectoryShare, SocketDeviceConfiguration, StorageDeviceConfiguration,
    VirtioFileSystemDeviceConfiguration, VirtualMachine, VirtualMachineConfiguration,
};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

const DISK_BYTES: u64 = 256 << 20;
const MEMORY: u64 = 1 << 30;
const CMDLINE: &str = "console=hvc0 rdinit=/init loglevel=4 panic=-1";

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let result = match args.as_slice() {
        [_, m] if m == "validate" => validate(),
        [_, m, d, s] if m == "save" || m == "restore" => match s.parse::<u64>() {
            Ok(run_s) if m == "save" => save(Path::new(d), run_s).await,
            Ok(run_s) => restore(Path::new(d), run_s).await,
            Err(e) => Err(e.into()),
        },
        _ => Err("usage: checkpoint_vz_spike validate | save|restore <dir> <run_s>".into()),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

/// What a restore needs to rebuild the saved machine (`spike.txt`).
struct Spec {
    kernel: PathBuf,
    initrd: PathBuf,
    vcpus: usize,
    memory: u64,
    machine_id: Vec<u8>,
    last_tick: String,
}

impl Spec {
    fn write(&self, path: &Path) -> Result<()> {
        let hex: String = self.machine_id.iter().fold(String::new(), |mut s, b| {
            use std::fmt::Write;
            let _ = write!(s, "{b:02x}");
            s
        });
        fs::write(
            path,
            format!(
                "kernel={}\ninitrd={}\nvcpus={}\nmemory={}\nmachine_id={hex}\nlast_tick={}\n",
                self.kernel.display(),
                self.initrd.display(),
                self.vcpus,
                self.memory,
                self.last_tick
            ),
        )?;
        Ok(())
    }

    fn read(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)?;
        let kv: BTreeMap<&str, &str> = text.lines().filter_map(|l| l.split_once('=')).collect();
        let get = |k: &str| {
            kv.get(k)
                .copied()
                .ok_or_else(|| format!("spike.txt: no {k}"))
        };
        let hex = get("machine_id")?;
        Ok(Self {
            kernel: get("kernel")?.into(),
            initrd: get("initrd")?.into(),
            vcpus: get("vcpus")?.parse()?,
            memory: get("memory")?.parse()?,
            machine_id: (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
                .collect::<std::result::Result<_, _>>()?,
            last_tick: get("last_tick")?.into(),
        })
    }
}

/// The production device set's base: one disk, the console, entropy.
/// Returns the configuration, its platform's machine identifier and the
/// console.
fn base_config(
    spec: &Spec,
    disk: &Path,
    machine_id: Option<&[u8]>,
) -> Result<(
    VirtualMachineConfiguration,
    Vec<u8>,
    SerialPortConfiguration,
)> {
    let mut boot = LinuxBootLoader::new(&spec.kernel)?;
    boot.set_initial_ramdisk(&spec.initrd)
        .set_command_line(CMDLINE);
    let platform = GenericPlatform::new()?;
    if let Some(id) = machine_id {
        platform.set_machine_identifier(id)?;
    }
    let id = platform.machine_identifier();
    let console = SerialPortConfiguration::virtio_console()?;
    let mut config = VirtualMachineConfiguration::new()?;
    config
        .set_cpu_count(spec.vcpus)
        .set_memory_size(spec.memory)
        .set_platform(platform)
        .set_boot_loader(boot)
        .add_storage_device(StorageDeviceConfiguration::disk_image(disk, false)?)
        .add_entropy_device(EntropyDeviceConfiguration::new()?);
    Ok((config, id, console))
}

/// Which of the devices a production VZ machine attaches allow save/restore.
fn validate() -> Result<()> {
    let dir = std::env::temp_dir().join(format!("vz-validate-{}", std::process::id()));
    fs::create_dir_all(&dir)?;
    let disk = dir.join("disk.img");
    fs::File::create(&disk)?.set_len(DISK_BYTES)?;
    let spec = Spec {
        kernel: env_path("SPIKE_KERNEL").map_or_else(default_kernel, Ok)?,
        initrd: disk.clone(),
        vcpus: 2,
        memory: MEMORY,
        machine_id: Vec::new(),
        last_tick: String::new(),
    };
    let variants: [&str; 10] = [
        "base (disk, console, entropy)",
        "+ vsock",
        "+ NAT network",
        "+ file-handle network",
        "+ VirtioFS share",
        "+ Rosetta share",
        "+ memory balloon",
        "+ second serial port",
        "+ nested virtualization",
        "production System VM set",
    ];
    for (i, name) in variants.iter().enumerate() {
        let (mut config, _, console) = base_config(&spec, &disk, None)?;
        let all = i == 9;
        let mut keep: Vec<SerialPortConfiguration> = Vec::new();
        let mut fds: Vec<RawFd> = Vec::new();
        if i == 1 || all {
            config.add_socket_device(SocketDeviceConfiguration::new()?);
        }
        if i == 2 {
            config.add_network_device(NetworkDeviceConfiguration::nat()?);
        }
        if i == 3 || all {
            let mut pair = [0; 2];
            // SAFETY: `pair` is a valid 2-element array.
            if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, pair.as_mut_ptr()) }
                != 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            fds.extend(pair);
            config.add_network_device(NetworkDeviceConfiguration::file_handle(pair[0])?);
        }
        if i == 4 || all {
            let mut fs_dev = VirtioFileSystemDeviceConfiguration::new("share")?;
            fs_dev.set_share(SingleDirectoryShare::new(SharedDirectory::new(
                &dir, false,
            )?)?);
            config.add_directory_share(fs_dev);
        }
        if i == 5 || all {
            let mut fs_dev = VirtioFileSystemDeviceConfiguration::new("rosetta")?;
            fs_dev.set_share(LinuxRosettaDirectoryShare::new()?);
            config.add_directory_share(fs_dev);
        }
        if i == 6 || all {
            config.add_memory_balloon_device(MemoryBalloonDeviceConfiguration::new()?);
        }
        if i == 7 || all {
            keep.push(SerialPortConfiguration::virtio_console()?);
        }
        if i == 8 || all {
            // Not settable after `set_platform`; rebuild with it enabled.
            let platform = GenericPlatform::new()?;
            platform.set_nested_virt_enabled(GenericPlatform::is_nested_virt_supported());
            config.set_platform(platform);
        }
        config.add_serial_port(console);
        for port in keep {
            config.add_serial_port(port);
        }
        let verdict = match (config.validate(), config.validate_save_restore()) {
            (Err(e), _) => format!("invalid configuration: {e}"),
            (Ok(()), Ok(())) => "save/restore supported".into(),
            (Ok(()), Err(e)) => format!("UNSUPPORTED: {e}"),
        };
        println!("{name:<32} {verdict}");
        for fd in fds {
            // SAFETY: this probe owns the socketpair fds.
            unsafe { libc::close(fd) };
        }
    }
    let _ = fs::remove_dir_all(&dir);
    Ok(())
}

async fn save(dir: &Path, run_s: u64) -> Result<()> {
    fs::create_dir_all(dir)?;
    let mut spec = Spec {
        kernel: env_path("SPIKE_KERNEL").map_or_else(default_kernel, Ok)?,
        initrd: env_path("SPIKE_INITRD")
            .unwrap_or_else(|| dir.join("..").join("initramfs.cpio.gz")),
        vcpus: std::env::var("SPIKE_VCPUS").map_or(Ok(4), |v| v.parse())?,
        memory: MEMORY,
        machine_id: Vec::new(),
        last_tick: String::new(),
    };
    let disk = dir.join("live-disk.img");
    fs::File::create(&disk)?.set_len(DISK_BYTES)?;
    let (config, id, port) = base_config(&spec, &disk, None)?;
    spec.machine_id = id;
    config.validate_save_restore()?;
    let (vm, console) = build(config, port)?;

    let t = Instant::now();
    vm.start().await?;
    console.release_guest_ends();
    console.wait_for(|l| l.starts_with("tick "), Duration::from_secs(30))?;
    println!(
        "[save] booted, first tick {:.0} ms after start",
        ms(t.elapsed())
    );
    let jitter = Duration::from_micros(u64::from(std::process::id()) * 7919 % 900_000);
    tokio::time::sleep(Duration::from_secs(run_s) + jitter).await;

    let t0 = Instant::now();
    vm.pause().await?;
    let t_pause = t0.elapsed();
    let state = dir.join("state.vzvmsave");
    vm.save_state(&state).await?;
    let t_save = t0.elapsed();
    fs::File::open(&disk)?.sync_all()?;
    clone(&disk, &dir.join("disk0.img"))?;
    let t_disk = t0.elapsed();
    spec.last_tick = console
        .last(|l| l.starts_with("tick "))
        .ok_or("no tick before the checkpoint")?;
    vm.stop().await?;
    spec.write(&dir.join("spike.txt"))?;
    fs::remove_file(&disk)?;

    println!(
        "[save] pause {:.1} ms, save {:.0} ms ({} MiB state file), disk fsync+clone {:.1} ms, total {:.0} ms",
        ms(t_pause),
        ms(t_save.saturating_sub(t_pause)),
        fs::metadata(&state)?.len() >> 20,
        ms(t_disk.saturating_sub(t_save)),
        ms(t_disk),
    );
    println!("[save] last tick before checkpoint: {}", spec.last_tick);
    let fails = console
        .lines()
        .iter()
        .filter(|l| l.starts_with("FAIL"))
        .count();
    if fails > 0 {
        return Err(format!("guest reported {fails} FAIL lines before the checkpoint").into());
    }
    Ok(())
}

async fn restore(dir: &Path, run_s: u64) -> Result<()> {
    let mut spec = Spec::read(&dir.join("spike.txt"))?;
    let saved = Tick::parse(&spec.last_tick).ok_or("bad saved tick")?;
    let run = dir.join(format!("run-{}", std::process::id()));
    fs::create_dir_all(&run)?;
    let disk = run.join("disk0.img");
    let mut state = dir.join("state.vzvmsave");
    let tamper = std::env::var("SPIKE_TAMPER").unwrap_or_default();
    match tamper.as_str() {
        "disk" => fs::File::create(&disk)?.set_len(DISK_BYTES)?,
        _ => clone(&dir.join("disk0.img"), &disk)?,
    }
    if tamper == "state" {
        let copy = run.join("state.vzvmsave");
        let mut bytes = fs::read(&state)?;
        let at = bytes.len() / 2;
        bytes[at] ^= 0x01;
        fs::write(&copy, bytes)?;
        state = copy;
    }
    if tamper == "memory" {
        spec.memory += 512 << 20;
    }
    let id = (tamper != "machine-id").then_some(spec.machine_id.as_slice());
    let (config, _, port) = base_config(&spec, &disk, id)?;
    let (vm, console) = build(config, port)?;

    let t = Instant::now();
    let restored = vm.restore_state(&state).await;
    let t_restore = t.elapsed();
    if let Err(e) = restored {
        let _ = fs::remove_dir_all(&run);
        return Err(format!("restore_state: {e}").into());
    }
    console.release_guest_ends();
    vm.resume().await?;
    let t_resume = t.elapsed();
    println!(
        "[restore] restore_state {:.0} ms, resume {:.1} ms",
        ms(t_restore),
        ms(t_resume.saturating_sub(t_restore))
    );
    let first = console.wait_for(|l| l.starts_with("tick "), Duration::from_secs(5));
    let first_ms = ms(t.elapsed());
    tokio::time::sleep(Duration::from_secs(run_s)).await;
    let _ = vm.stop().await;
    let _ = fs::remove_dir_all(&run);
    let first = first?;

    let lines = console.lines();
    let ticks: Vec<Tick> = lines.iter().filter_map(|l| Tick::parse(l)).collect();
    let last = ticks.last().ok_or("no tick after restore")?;
    let head = Tick::parse(&first).ok_or("bad tick")?;
    println!("[restore] saved  {}", spec.last_tick);
    println!("[restore] first  {first} ({first_ms:.0} ms after restore_state was called)");
    println!(
        "[restore] clock across the checkpoint: monotonic +{} ms, realtime +{} s",
        head.mono_ms.wrapping_sub(saved.mono_ms),
        head.real.wrapping_sub(saved.real),
    );

    let mut problems = Vec::new();
    for l in lines.iter().filter(|l| l.starts_with("FAIL")).take(5) {
        problems.push(format!("guest: {l}"));
    }
    if head.n != saved.n + 1 {
        problems.push(format!("tick counter jumped {} -> {}", saved.n, head.n));
    }
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
        "[restore] {} ticks, ops per thread after restore {done:?}",
        ticks.len()
    );
    if problems.is_empty() {
        println!("[restore] OK");
        Ok(())
    } else {
        Err(problems.join("; ").into())
    }
}

/// Adds the console last (as production does) and builds the VM.
fn build(
    mut config: VirtualMachineConfiguration,
    port: SerialPortConfiguration,
) -> Result<(VirtualMachine, Console)> {
    let console = Console::drain(&port)?;
    config.add_serial_port(port);
    Ok((config.build()?, console))
}

/// One `tick` line from the guest's `/init`.
struct Tick {
    n: u64,
    mono_ms: u64,
    real: u64,
    ops: [u64; 4],
    fails: u64,
}

impl Tick {
    fn parse(line: &str) -> Option<Self> {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() != 13 || f[0] != "tick" || f[2] != "mono_ms" || f[6] != "ops" {
            return None;
        }
        let n = |i: usize| f[i].parse::<u64>().ok();
        Some(Self {
            n: n(1)?,
            mono_ms: n(3)?,
            real: n(5)?,
            ops: [n(7)?, n(8)?, n(9)?, n(10)?],
            fails: n(12)?,
        })
    }
}

/// Drains the guest console pipe on a thread (a VZ console nobody reads
/// wedges the VM) and keeps its lines.
#[derive(Clone)]
struct Console {
    lines: Arc<Mutex<Vec<String>>>,
    guest_ends: Arc<Mutex<Option<(RawFd, RawFd)>>>,
}

impl Console {
    fn drain(port: &SerialPortConfiguration) -> Result<Self> {
        let fd = port.read_fd().ok_or("console has no read end")?;
        let console = Self {
            lines: Arc::default(),
            guest_ends: Arc::new(Mutex::new(port.guest_fds())),
        };
        let lines = Arc::clone(&console.lines);
        // SAFETY: the probe owns the host read end; the reader closes it.
        let file = unsafe { fs::File::from_raw_fd(fd) };
        std::thread::spawn(move || {
            for line in BufReader::new(file)
                .lines()
                .map_while(std::result::Result::ok)
            {
                let line = line.trim_end_matches('\r').to_string();
                println!("  guest| {line}");
                lines.lock().unwrap().push(line);
            }
        });
        Ok(console)
    }

    /// Closes the VZ-facing pipe ends once the helper holds its copies.
    fn release_guest_ends(&self) {
        let ends = self.guest_ends.lock().unwrap().take();
        if let Some((r, w)) = ends {
            // SAFETY: the probe owns both ends and closes them once.
            unsafe {
                libc::close(r);
                libc::close(w);
            }
        }
    }

    fn lines(&self) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }

    fn last(&self, f: impl Fn(&str) -> bool) -> Option<String> {
        self.lines
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|l| f(l))
            .cloned()
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
