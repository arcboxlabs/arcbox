//! Guest-side check for `qemu_system_vm_probe`: does the vCPU run code a
//! virtio-fs back-end wrote into a page the vCPU had already executed?
//!
//! ```text
//! exec-coherence <routine file> <dir of N.bin routine files> <rounds>
//! ```
//!
//! Every routine file holds `mov w0, #2; ret`. Three ways to reach it:
//!
//! - `buffered`, `direct`: write `mov w0, #1; ret` into an anonymous page
//!   and call it, read the routine into the same page, do the maintenance
//!   the architecture asks for before executing data another observer wrote
//!   (DC CVAU, IC IVAU, ISB), and call it again. A buffered read has the
//!   guest CPU copy the data in; with `O_DIRECT` FUSE hands the page itself
//!   to the device and the back-end writes it.
//! - `share`: what exec does. Run code in an anonymous page, then map a
//!   routine file from the share over it (`MAP_FIXED`) and call it, leaving
//!   instruction-cache maintenance to the kernel's fault path. The page the
//!   anonymous mapping frees is the first the page cache takes, so the file
//!   usually lands in the frame that just ran other code. One round per
//!   file, pinned to one CPU.
//!
//! Prints `exec-coherence buffered=<n> direct=<n> share-stale=<k>/<rounds>`;
//! the coherent answer is 2, 2 and 0. Not part of any crate: the probe
//! builds it with `rustc`.

use std::arch::asm;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;

const PAGE: usize = 4096;
const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const PROT_EXEC: i32 = 4;
const MAP_PRIVATE: i32 = 2;
const MAP_FIXED: i32 = 0x10;
const MAP_ANONYMOUS: i32 = 0x20;
/// arm64's value; it differs from x86's.
const O_DIRECT: i32 = 0o200_000;

unsafe extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
    fn sched_setaffinity(pid: i32, size: usize, mask: *const u64) -> i32;
}

/// `mov w0, #imm; ret`.
fn ret_imm(imm: u32) -> [u8; 8] {
    let mut code = [0; 8];
    code[..4].copy_from_slice(&(0x5280_0000 | (imm << 5)).to_le_bytes());
    code[4..].copy_from_slice(&0xd65f_03c0_u32.to_le_bytes());
    code
}

/// `pages` fresh private anonymous read-write pages.
fn anonymous(pages: usize) -> *mut u8 {
    // SAFETY: a new mapping chosen by the kernel; nothing else uses it.
    let area = unsafe {
        mmap(
            std::ptr::null_mut(),
            pages * PAGE,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert!(!area.is_null() && area as isize != -1, "mmap failed");
    area
}

/// Writes `mov w0, #imm; ret` at `page`, synchronizes it with the
/// instruction stream, and calls it.
fn run(page: *mut u8, imm: u32) -> u32 {
    // SAFETY: `page` is a mapped read-write page that nothing else aliases.
    unsafe { std::slice::from_raw_parts_mut(page, 8) }.copy_from_slice(&ret_imm(imm));
    call(page)
}

/// Makes `page` executable, synchronizes it with the instruction stream,
/// calls it, and makes it writable again.
fn call(page: *mut u8) -> u32 {
    // SAFETY: `page` is a whole mapped page holding a `mov; ret` routine;
    // the cache operations take addresses inside it.
    unsafe {
        assert_eq!(mprotect(page, PAGE, PROT_READ | PROT_EXEC), 0);
        for line in (0..PAGE).step_by(64) {
            asm!("dc cvau, {0}", in(reg) page.add(line));
        }
        asm!("dsb ish");
        for line in (0..PAGE).step_by(64) {
            asm!("ic ivau, {0}", in(reg) page.add(line));
        }
        asm!("dsb ish", "isb");
        let routine: extern "C" fn() -> u32 = std::mem::transmute(page);
        let result = routine();
        assert_eq!(mprotect(page, PAGE, PROT_READ | PROT_WRITE), 0);
        result
    }
}

fn read_round(path: &str, flags: i32) -> u32 {
    let page = anonymous(1);
    assert_eq!(run(page, 1), 1);
    // SAFETY: the page is mapped read-write and nothing else aliases it.
    let bytes = unsafe { std::slice::from_raw_parts_mut(page, PAGE) };
    OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(path)
        .and_then(|mut file| file.read_exact(bytes))
        .expect("read the routine");
    assert_eq!(bytes[..8], ret_imm(2), "the file holds mov w0, #2; ret");
    call(page)
}

/// Rounds in which code mapped from the share ran something else.
fn share_rounds(dir: &str, rounds: u32) -> u32 {
    let cpu0 = 1u64;
    // SAFETY: the mask outlives the call.
    assert_eq!(unsafe { sched_setaffinity(0, 8, &raw const cpu0) }, 0);
    let mut stale = 0;
    for round in 0..rounds {
        // The second page keeps the page table alive across the remap.
        let area = anonymous(2);
        assert_eq!(run(area, 100 + round), 100 + round);
        let file = File::open(format!("{dir}/{round}.bin")).expect("open a routine file");
        // SAFETY: replaces the first page of `area`, which this function owns.
        let mapped = unsafe {
            mmap(
                area,
                PAGE,
                PROT_READ | PROT_EXEC,
                MAP_PRIVATE | MAP_FIXED,
                file.as_raw_fd(),
                0,
            )
        };
        assert_eq!(mapped, area, "map the routine file");
        // SAFETY: the page maps a file holding `mov w0, #2; ret`.
        let routine: extern "C" fn() -> u32 = unsafe { std::mem::transmute(area) };
        stale += u32::from(routine() != 2);
        // SAFETY: unmaps `area`, which this function owns.
        unsafe { munmap(area, 2 * PAGE) };
    }
    stale
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, routine, dir, rounds] = args.as_slice() else {
        panic!("usage: exec-coherence <routine file> <dir> <rounds>");
    };
    let rounds = rounds.parse().expect("rounds");
    let buffered = read_round(routine, 0);
    let direct = read_round(routine, O_DIRECT);
    let stale = share_rounds(dir, rounds);
    println!("exec-coherence buffered={buffered} direct={direct} share-stale={stale}/{rounds}");
}
