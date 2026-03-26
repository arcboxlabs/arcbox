//! Boot loader configurations.

use crate::error::{VZError, VZResult};
use crate::ffi::{get_class, nsurl_file_path, release};
use crate::{msg_send, msg_send_void};
use objc2::runtime::AnyObject;
use std::path::Path;

/// Trait for boot loader configurations.
pub trait BootLoader {
    /// Returns the underlying Objective-C object pointer.
    fn as_ptr(&self) -> *mut AnyObject;
}

/// A boot loader for Linux kernels.
///
/// This boot loader loads a Linux kernel image directly, without
/// requiring UEFI or other firmware.
pub struct LinuxBootLoader {
    inner: *mut AnyObject,
}

// SAFETY: Inner ObjC pointer is only used via msg_send! which dispatches to the ObjC runtime. Not shared across threads without external synchronization.
unsafe impl Send for LinuxBootLoader {}

impl LinuxBootLoader {
    /// Creates a new Linux boot loader with the specified kernel path.
    ///
    /// # Arguments
    ///
    /// * `kernel_path` - Path to the Linux kernel image (uncompressed ARM64 Image format)
    ///
    /// # Errors
    ///
    /// Returns an error if the kernel path is invalid or the boot loader
    /// cannot be created.
    pub fn new(kernel_path: impl AsRef<Path>) -> VZResult<Self> {
        let path = kernel_path.as_ref();
        if !path.exists() {
            return Err(VZError::NotFound(path.display().to_string()));
        }

        let path_str = path.to_string_lossy();

        // SAFETY: ObjC alloc/init pattern on valid VZLinuxBootLoader class. Result is checked non-null.
        unsafe {
            let cls = get_class("VZLinuxBootLoader").ok_or_else(|| VZError::Internal {
                code: -1,
                message: "VZLinuxBootLoader class not found".into(),
            })?;
            let kernel_url = nsurl_file_path(&path_str);
            let alloc = msg_send!(cls, alloc);
            let obj = msg_send!(alloc, initWithKernelURL: kernel_url);

            if obj.is_null() {
                return Err(VZError::InvalidConfiguration(format!(
                    "Failed to create boot loader for kernel: {path_str}"
                )));
            }

            Ok(Self { inner: obj })
        }
    }

    /// Sets the initial ramdisk (initrd) path.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the initrd/initramfs image
    pub fn set_initial_ramdisk(&mut self, path: impl AsRef<Path>) -> &mut Self {
        let path = path.as_ref();
        if path.exists() {
            let path_str = path.to_string_lossy();
            // SAFETY: self.inner is a valid VZLinuxBootLoader. NSURL is created from a validated path.
            unsafe {
                let url = nsurl_file_path(&path_str);
                msg_send_void!(self.inner, setInitialRamdiskURL: url);
            }
        }
        self
    }

    /// Sets the kernel command line arguments.
    ///
    /// # Arguments
    ///
    /// * `cmdline` - Kernel command line string (e.g., "console=hvc0 root=/dev/vda")
    pub fn set_command_line(&mut self, cmdline: &str) -> &mut Self {
        // SAFETY: self.inner is a valid VZLinuxBootLoader. NSString is created from a valid Rust str.
        unsafe {
            let s = crate::ffi::nsstring(cmdline);
            msg_send_void!(self.inner, setCommandLine: s);
        }
        self
    }
}

impl BootLoader for LinuxBootLoader {
    fn as_ptr(&self) -> *mut AnyObject {
        self.inner
    }
}

impl Drop for LinuxBootLoader {
    fn drop(&mut self) {
        if !self.inner.is_null() {
            release(self.inner);
        }
    }
}
