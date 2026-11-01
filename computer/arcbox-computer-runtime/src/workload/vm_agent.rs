//! The `vm-agent` binary every computer boots as PID 1.

use std::path::{Path, PathBuf};

use super::WorkloadError;

/// ELF `e_machine` of the guest CPU: Linux on the host's architecture.
#[cfg(target_arch = "aarch64")]
const GUEST_MACHINE: u16 = 183; // EM_AARCH64
#[cfg(target_arch = "x86_64")]
const GUEST_MACHINE: u16 = 62; // EM_X86_64
#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
const GUEST_MACHINE: u16 = 0;

/// Program header type naming a dynamic loader.
const PT_INTERP: u32 = 3;

/// Where the daemon stages `vm-agent`: `<data_dir>/bin/vm-agent`.
///
/// It sits next to `bin/arcbox-agent`. The app bundle ships both in `Contents/Resources/bin`
/// and the daemon seeds them into the data directory at startup; a dev or
/// e2e daemon stages the musl build there by hand.
pub fn vm_agent_path(data_dir: &Path) -> PathBuf {
    data_dir.join("bin/vm-agent")
}

/// Check that `path` is a `vm-agent` a computer can boot: a statically
/// linked 64-bit little-endian Linux ELF for the guest CPU.
///
/// A host build (Mach-O) or a glibc build would be written into the rootfs
/// and fail only at boot, as a kernel panic with no agent to report it.
pub fn check_vm_agent(path: &Path) -> Result<(), WorkloadError> {
    let fail = |why: &str| {
        WorkloadError::Agent(format!(
            "vm-agent at {} {why}; build it with `cargo build -p arcbox-vm-agent --release \
             --target {}-unknown-linux-musl`",
            path.display(),
            std::env::consts::ARCH
        ))
    };
    let elf = std::fs::read(path).map_err(|error| fail(&format!("cannot be read ({error})")))?;
    if elf.get(..4) != Some(b"\x7fELF".as_slice()) {
        return Err(fail("is not an ELF binary (a host build?)"));
    }
    // EI_CLASS 2 = 64-bit, EI_DATA 1 = little-endian.
    if elf.get(4..6) != Some([2, 1].as_slice()) {
        return Err(fail("is not a 64-bit little-endian ELF"));
    }
    let u16_at = |offset: usize| {
        elf.get(offset..offset.checked_add(2)?)
            .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
    };
    let u64_at = |offset: usize| {
        elf.get(offset..offset.checked_add(8)?)
            .map(|bytes| u64::from_le_bytes(bytes.try_into().expect("8 bytes")))
    };
    if u16_at(18) != Some(GUEST_MACHINE) {
        return Err(fail(&format!(
            "is built for another CPU (e_machine {:?})",
            u16_at(18)
        )));
    }
    let truncated = || fail("has a truncated program header table");
    let table = usize::try_from(u64_at(32).ok_or_else(truncated)?).map_err(|_| truncated())?;
    let entry_size = usize::from(u16_at(54).ok_or_else(truncated)?);
    let entries = usize::from(u16_at(56).ok_or_else(truncated)?);
    for index in 0..entries {
        let offset = index
            .checked_mul(entry_size)
            .and_then(|offset| offset.checked_add(table))
            .ok_or_else(truncated)?;
        let kind = elf
            .get(offset..offset.checked_add(4).ok_or_else(truncated)?)
            .ok_or_else(truncated)?;
        if u32::from_le_bytes(kind.try_into().expect("4 bytes")) == PT_INTERP {
            return Err(fail(
                "is dynamically linked; the rootfs may have no loader for it",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 64-bit ELF header followed by one program header per `kinds`.
    fn elf(machine: u16, kinds: &[u32]) -> Vec<u8> {
        let mut elf = vec![0u8; 64];
        elf[..6].copy_from_slice(b"\x7fELF\x02\x01");
        elf[18..20].copy_from_slice(&machine.to_le_bytes());
        elf[32..40].copy_from_slice(&64u64.to_le_bytes());
        elf[54..56].copy_from_slice(&56u16.to_le_bytes());
        elf[56..58].copy_from_slice(&u16::try_from(kinds.len()).unwrap().to_le_bytes());
        for kind in kinds {
            let mut header = vec![0u8; 56];
            header[..4].copy_from_slice(&kind.to_le_bytes());
            elf.extend(header);
        }
        elf
    }

    fn check(bytes: &[u8]) -> Result<(), String> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vm-agent");
        std::fs::write(&path, bytes).unwrap();
        check_vm_agent(&path).map_err(|error| error.to_string())
    }

    #[test]
    fn a_static_guest_elf_passes() {
        // PT_LOAD, PT_GNU_STACK: what a static musl build carries.
        check(&elf(GUEST_MACHINE, &[1, 0x6474_e551])).unwrap();
    }

    #[test]
    fn binaries_that_cannot_boot_are_rejected() {
        let other_machine = if GUEST_MACHINE == 183 { 62 } else { 183 };
        let mut truncated = elf(GUEST_MACHINE, &[1, 1]);
        truncated.truncate(64 + 56 + 2);
        let mut far_table = elf(GUEST_MACHINE, &[1]);
        far_table[32..40].copy_from_slice(&u64::MAX.to_le_bytes());
        for (bytes, expected) in [
            (b"\xcf\xfa\xed\xfe".to_vec(), "is not an ELF binary"),
            (b"\x7fELF\x01\x01".to_vec(), "64-bit little-endian"),
            (elf(other_machine, &[1]), "another CPU"),
            (elf(GUEST_MACHINE, &[1, PT_INTERP]), "dynamically linked"),
            (truncated, "truncated"),
            (far_table, "truncated"),
        ] {
            let error = check(&bytes).unwrap_err();
            assert!(error.contains(expected), "{expected}: {error}");
            assert!(error.contains("unknown-linux-musl"), "{error}");
        }
        let missing = check_vm_agent(Path::new("/nonexistent/vm-agent")).unwrap_err();
        assert!(missing.to_string().contains("cannot be read"), "{missing}");
    }

    #[test]
    fn the_agent_is_staged_next_to_the_guest_agent() {
        assert_eq!(
            vm_agent_path(Path::new("/Users/me/.arcbox")),
            Path::new("/Users/me/.arcbox/bin/vm-agent")
        );
    }
}
