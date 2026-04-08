//! vmnet.framework backend implementation.
//!
//! This module provides a high-level Rust interface to Apple's vmnet.framework
//! for creating virtual network interfaces on macOS.
//!
//! # Modes
//!
//! - **Shared (NAT)**: VMs share host network via NAT, with DHCP
//! - **Host-only**: Isolated network between VMs and host
//! - **Bridged**: VMs appear as separate devices on physical network
//!
//! # Requirements
//!
//! - macOS 11.0 or later
//! - Privileged user or appropriate entitlements

use std::ffi::CString;
use std::net::Ipv4Addr;
use std::os::raw::c_int;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

use super::vmnet_ffi::*;
use crate::backend::NetworkBackend;
use crate::error::{NetError, Result};

/// Information returned by vmnet_start_interface completion handler.
///
/// Only available when the `vmnet` feature is enabled (proper block ABI).
#[derive(Debug, Clone)]
pub struct VmnetInterfaceInfo {
    /// MAC address assigned by vmnet.
    pub mac: [u8; 6],
    /// MTU reported by vmnet.
    pub mtu: u16,
    /// Maximum Ethernet frame size.
    pub max_packet_size: usize,
}

/// Default MTU size.
const DEFAULT_MTU: u16 = 1500;

/// Default maximum packet size (Ethernet frame + VLAN tag).
/// The actual value is returned by vmnet_start_interface completion handler,
/// but without proper Objective-C block support, we use this default.
const DEFAULT_MAX_PACKET_SIZE: usize = 1518;

/// vmnet interface configuration.
#[derive(Debug, Clone)]
pub struct VmnetConfig {
    /// Operating mode.
    pub mode: VmnetMode,
    /// MAC address (auto-generated if None).
    pub mac: Option<[u8; 6]>,
    /// MTU size.
    pub mtu: u16,
    /// Bridge interface name (for bridged mode).
    pub bridge_interface: Option<String>,
    /// Start IP address for DHCP range (shared mode).
    pub dhcp_start: Option<Ipv4Addr>,
    /// End IP address for DHCP range (shared mode).
    pub dhcp_end: Option<Ipv4Addr>,
    /// Subnet mask (shared mode).
    pub subnet_mask: Option<Ipv4Addr>,
    /// Enable isolation (host mode).
    pub isolated: bool,
}

impl Default for VmnetConfig {
    fn default() -> Self {
        Self {
            mode: VmnetMode::Shared,
            mac: None,
            mtu: DEFAULT_MTU,
            bridge_interface: None,
            dhcp_start: None,
            dhcp_end: None,
            subnet_mask: None,
            isolated: false,
        }
    }
}

impl VmnetConfig {
    /// Creates a new shared (NAT) mode configuration.
    #[must_use]
    pub fn shared() -> Self {
        Self {
            mode: VmnetMode::Shared,
            ..Default::default()
        }
    }

    /// Creates a new host-only mode configuration.
    #[must_use]
    pub fn host_only() -> Self {
        Self {
            mode: VmnetMode::HostOnly,
            ..Default::default()
        }
    }

    /// Creates a new bridged mode configuration.
    #[must_use]
    pub fn bridged(interface: impl Into<String>) -> Self {
        Self {
            mode: VmnetMode::Bridged,
            bridge_interface: Some(interface.into()),
            ..Default::default()
        }
    }

    /// Sets the MAC address.
    #[must_use]
    pub fn with_mac(mut self, mac: [u8; 6]) -> Self {
        self.mac = Some(mac);
        self
    }

    /// Sets the MTU.
    #[must_use]
    pub fn with_mtu(mut self, mtu: u16) -> Self {
        self.mtu = mtu;
        self
    }

    /// Sets the DHCP range (shared mode).
    #[must_use]
    pub fn with_dhcp_range(mut self, start: Ipv4Addr, end: Ipv4Addr) -> Self {
        self.dhcp_start = Some(start);
        self.dhcp_end = Some(end);
        self
    }

    /// Sets the subnet mask (shared mode).
    #[must_use]
    pub fn with_subnet_mask(mut self, mask: Ipv4Addr) -> Self {
        self.subnet_mask = Some(mask);
        self
    }

    /// Enables isolation (host mode).
    #[must_use]
    pub fn with_isolation(mut self, isolated: bool) -> Self {
        self.isolated = isolated;
        self
    }
}

/// vmnet operating mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VmnetMode {
    /// Shared (NAT) mode.
    #[default]
    Shared,
    /// Host-only mode.
    HostOnly,
    /// Bridged mode.
    Bridged,
}

impl From<VmnetMode> for VmnetOperatingMode {
    fn from(mode: VmnetMode) -> Self {
        match mode {
            VmnetMode::Shared => Self::Shared,
            VmnetMode::HostOnly => Self::Host,
            VmnetMode::Bridged => Self::Bridged,
        }
    }
}

/// Return value from the interface start helpers: (handle, mac, mtu, max_pkt, info).
type StartResult = (
    VmnetInterfaceRef,
    [u8; 6],
    u16,
    usize,
    Option<VmnetInterfaceInfo>,
);

/// vmnet backend for macOS.
///
/// Provides network connectivity for VMs using Apple's vmnet.framework.
pub struct Vmnet {
    /// vmnet interface handle.
    interface: VmnetInterfaceRef,
    /// Dispatch queue for vmnet operations.
    queue: DispatchQueue,
    /// MAC address.
    mac: [u8; 6],
    /// MTU size.
    mtu: u16,
    /// Maximum packet size.
    max_packet_size: usize,
    /// Running state.
    running: AtomicBool,
    /// Parsed interface parameters from completion handler (`vmnet` feature only).
    interface_info: Option<VmnetInterfaceInfo>,
}

// Safety: Vmnet uses thread-safe primitives and vmnet.framework is thread-safe.
unsafe impl Send for Vmnet {}
unsafe impl Sync for Vmnet {}

impl Vmnet {
    /// Creates a new vmnet interface with the given configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The vmnet interface cannot be created
    /// - The user doesn't have sufficient privileges
    /// - The configuration is invalid
    pub fn new(config: VmnetConfig) -> Result<Self> {
        // Create dispatch queue for vmnet operations.
        let queue_label = CString::new("com.arcbox.vmnet").unwrap();

        // Safety: dispatch_queue_create is safe to call with valid parameters.
        let queue = unsafe { dispatch_queue_create(queue_label.as_ptr(), ptr::null()) };

        if queue.is_null() {
            return Err(NetError::config(
                "failed to create dispatch queue".to_string(),
            ));
        }

        // Build XPC configuration dictionary.
        // vmnet_start_interface requires xpc_object_t, NOT CFDictionaryRef.
        let config_dict: *mut std::ffi::c_void = unsafe {
            let dict = xpc_dictionary_create(ptr::null(), ptr::null(), 0);
            if dict.is_null() {
                dispatch_release(queue.cast());
                return Err(NetError::config(
                    "failed to create xpc config dictionary".to_string(),
                ));
            }

            // Set operating mode.
            let mode_value: u64 = VmnetOperatingMode::from(config.mode) as u64;
            xpc_dictionary_set_uint64(dict, vmnet_operation_mode_key, mode_value);

            // Set MAC address if provided.
            if let Some(mac) = config.mac {
                let mac_str = std::ffi::CString::new(format!(
                    "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                    mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
                ))
                .unwrap();
                xpc_dictionary_set_string(dict, vmnet_mac_address_key, mac_str.as_ptr());
            }

            // Set MTU.
            xpc_dictionary_set_uint64(dict, vmnet_mtu_key, u64::from(config.mtu));

            // Mode-specific configuration.
            match config.mode {
                VmnetMode::Shared => {
                    if let Some(start) = config.dhcp_start {
                        let s = std::ffi::CString::new(start.to_string()).unwrap();
                        xpc_dictionary_set_string(dict, vmnet_start_address_key, s.as_ptr());
                    }
                    if let Some(end) = config.dhcp_end {
                        let s = std::ffi::CString::new(end.to_string()).unwrap();
                        xpc_dictionary_set_string(dict, vmnet_end_address_key, s.as_ptr());
                    }
                    if let Some(mask) = config.subnet_mask {
                        let s = std::ffi::CString::new(mask.to_string()).unwrap();
                        xpc_dictionary_set_string(dict, vmnet_subnet_mask_key, s.as_ptr());
                    }
                }
                VmnetMode::HostOnly => {
                    if config.isolated {
                        xpc_dictionary_set_bool(dict, vmnet_enable_isolation_key, true);

                        // Generate unique interface ID for isolation.
                        let uuid_bytes = uuid::Uuid::new_v4();
                        let xpc_uuid = xpc_uuid_create(uuid_bytes.as_bytes().as_ptr());
                        xpc_dictionary_set_value(dict, vmnet_interface_id_key, xpc_uuid);
                        xpc_release(xpc_uuid);
                    }
                }
                VmnetMode::Bridged => {
                    if let Some(ref iface) = config.bridge_interface {
                        let s = std::ffi::CString::new(iface.as_str()).unwrap();
                        xpc_dictionary_set_string(dict, vmnet_shared_interface_name_key, s.as_ptr());
                    } else {
                        xpc_release(dict);
                        dispatch_release(queue.cast());
                        return Err(NetError::config(
                            "bridge mode requires interface name".to_string(),
                        ));
                    }
                }
            }

            dict
        };

        // Start the interface with the appropriate completion handler strategy.
        #[cfg(feature = "vmnet")]
        let (interface, mac, mtu, max_packet_size, interface_info) =
            { Self::start_with_completion_handler(config_dict, queue, &config)? };

        #[cfg(not(feature = "vmnet"))]
        let (interface, mac, mtu, max_packet_size, interface_info) =
            { Self::start_with_null_handler(config_dict, queue, &config)? };

        Ok(Self {
            interface,
            queue,
            mac,
            mtu,
            max_packet_size,
            running: AtomicBool::new(true),
            interface_info,
        })
    }

    /// Starts vmnet with a proper Objective-C block completion handler.
    ///
    /// Extracts MAC, MTU, and max_packet_size from the interface_param dictionary
    /// returned by vmnet. This path is only available when the `vmnet` feature is
    /// enabled (requires `com.apple.vm.networking` entitlement or root).
    #[cfg(feature = "vmnet")]
    fn start_with_completion_handler(
        config_dict: XpcObjectT,
        queue: DispatchQueue,
        config: &VmnetConfig,
    ) -> Result<StartResult> {
        use std::ffi::CStr;

        // SAFETY: dispatch_semaphore_create is safe with value 0.
        let sema = unsafe { dispatch_semaphore_create(0) };
        if sema.is_null() {
            unsafe {
                xpc_release(config_dict);
                dispatch_release(queue.cast());
            }
            return Err(NetError::config(
                "failed to create dispatch semaphore".to_string(),
            ));
        }

        // SAFETY: create_vmnet_completion_block takes a valid semaphore.
        let block = unsafe { create_vmnet_completion_block(sema) };

        // SAFETY: vmnet_start_interface with valid XPC config, queue, and block.
        let interface = unsafe { vmnet_start_interface(config_dict, queue, block) };

        // Wait for the completion handler to fire with a bounded timeout.
        // 10 seconds is generous; vmnet usually completes within milliseconds.
        let timeout = unsafe { dispatch_time(DISPATCH_TIME_NOW, 10 * NSEC_PER_SEC as i64) };
        // SAFETY: Valid semaphore and computed timeout.
        let wait_result = unsafe { dispatch_semaphore_wait(sema, timeout) };
        if wait_result != 0 {
            // Intentionally leak the block and semaphore. vmnet may still
            // invoke the completion handler after our timeout — releasing
            // the block now would cause a use-after-free when that happens.
            // A small leak is preferable to a crash. The dispatch queue is
            // also leaked to keep the handler's execution context valid.
            unsafe { xpc_release(config_dict) };
            return Err(NetError::config(
                "vmnet_start_interface timed out after 10s (completion handler never fired)"
                    .to_string(),
            ));
        }

        // SAFETY: After semaphore fires, the block's captured fields are populated.
        let (status, xpc_params) = unsafe {
            let b = block.cast::<VmnetCompletionBlock>();
            ((*b).status, (*b).interface_param)
        };

        // Release config dict now that vmnet has consumed it.
        unsafe { xpc_release(config_dict) };

        if !status.is_success() || interface.is_null() {
            unsafe {
                if !xpc_params.is_null() {
                    xpc_release(xpc_params);
                }
                _Block_release(block);
                dispatch_release(sema);
                dispatch_release(queue.cast());
            }
            return Err(NetError::config(format!(
                "vmnet_start_interface failed: {} (requires entitlement or root)",
                status.message()
            )));
        }

        // Parse interface parameters from the XPC dictionary.
        let mut info = VmnetInterfaceInfo {
            mac: [0u8; 6],
            mtu: DEFAULT_MTU,
            max_packet_size: DEFAULT_MAX_PACKET_SIZE,
        };

        if !xpc_params.is_null() {
            // Parse MAC address string (e.g. "aa:bb:cc:dd:ee:ff").
            let mac_key = c"mac_address";
            // SAFETY: xpc_dictionary_get_string returns a valid C string or NULL.
            let mac_ptr = unsafe { xpc_dictionary_get_string(xpc_params, mac_key.as_ptr()) };
            if !mac_ptr.is_null() {
                // SAFETY: mac_ptr is a valid C string from XPC.
                let mac_cstr = unsafe { CStr::from_ptr(mac_ptr) };
                if let Ok(mac_str) = mac_cstr.to_str() {
                    if let Ok(parsed) = super::parse_mac(mac_str) {
                        info.mac = parsed;
                    }
                }
            }

            // Parse MTU.
            let mtu_key = c"mtu";
            let mtu_val = unsafe { xpc_dictionary_get_uint64(xpc_params, mtu_key.as_ptr()) };
            if mtu_val > 0 {
                info.mtu = mtu_val as u16;
            }

            // Parse max packet size.
            let mps_key = c"max_packet_size";
            let mps_val = unsafe { xpc_dictionary_get_uint64(xpc_params, mps_key.as_ptr()) };
            if mps_val > 0 {
                info.max_packet_size = mps_val as usize;
            }

            // Note: xpc_params is owned by the block (retained in vmnet_block_invoke).
            // vmnet_block_dispose releases it when _Block_release runs below.
        }

        // Clean up the block (releases captured xpc_params) and semaphore.
        unsafe {
            _Block_release(block);
            dispatch_release(sema);
        }

        // If the user specified a MAC, prefer it. Otherwise use what vmnet returned.
        let mac = config.mac.unwrap_or(info.mac);
        let mtu = if config.mtu != DEFAULT_MTU {
            config.mtu
        } else {
            info.mtu
        };
        let max_packet_size = info.max_packet_size;

        // Update info to reflect final values.
        let final_info = VmnetInterfaceInfo {
            mac,
            mtu,
            max_packet_size,
        };

        Ok((interface, mac, mtu, max_packet_size, Some(final_info)))
    }

    /// Starts vmnet with NULL handler (synchronous, no interface_param).
    ///
    /// Fallback when the `vmnet` feature is not enabled. Uses default values
    /// for max_packet_size since the completion handler is not invoked.
    #[cfg(not(feature = "vmnet"))]
    fn start_with_null_handler(
        config_dict: XpcObjectT,
        queue: DispatchQueue,
        config: &VmnetConfig,
    ) -> Result<StartResult> {
        // SAFETY: vmnet_start_interface with NULL handler operates synchronously.
        let interface =
            unsafe { vmnet_start_interface(config_dict, queue, ptr::null()) };

        unsafe { xpc_release(config_dict) };

        if interface.is_null() {
            unsafe { dispatch_release(queue.cast()) };
            return Err(NetError::config(
                "failed to start vmnet interface (requires root or entitlements)".to_string(),
            ));
        }

        let mac = config.mac.unwrap_or_else(super::generate_mac);

        Ok((interface, mac, config.mtu, DEFAULT_MAX_PACKET_SIZE, None))
    }

    /// Returns interface parameters parsed from the vmnet completion handler.
    ///
    /// Only populated when the `vmnet` feature is enabled.
    #[must_use]
    pub fn interface_info(&self) -> Option<&VmnetInterfaceInfo> {
        self.interface_info.as_ref()
    }

    /// Creates a vmnet interface with default NAT (shared) configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if the interface cannot be created.
    pub fn new_shared() -> Result<Self> {
        Self::new(VmnetConfig::shared())
    }

    /// Creates a vmnet interface with host-only configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if the interface cannot be created.
    pub fn new_host_only() -> Result<Self> {
        Self::new(VmnetConfig::host_only())
    }

    /// Creates a vmnet interface bridged to the specified interface.
    ///
    /// # Errors
    ///
    /// Returns an error if the interface cannot be created.
    pub fn new_bridged(interface: &str) -> Result<Self> {
        Self::new(VmnetConfig::bridged(interface))
    }

    /// Returns true if the interface is running.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// Returns the maximum packet size.
    #[must_use]
    pub fn max_packet_size(&self) -> usize {
        self.max_packet_size
    }

    /// Reads a packet from the vmnet interface.
    ///
    /// # Errors
    ///
    /// Returns an error if the read operation fails.
    pub fn read_packet(&self, buf: &mut [u8]) -> Result<usize> {
        if !self.is_running() {
            return Err(NetError::config("interface not running".to_string()));
        }

        // Create iovec for the buffer.
        let mut iov = iovec {
            iov_base: buf.as_mut_ptr().cast(),
            iov_len: buf.len(),
        };

        // Create packet descriptor.
        let mut packet = VmnetPacket {
            vm_pkt_iov: &raw mut iov,
            vm_pkt_iovcnt: 1,
            vm_pkt_size: 0,
            vm_flags: 0,
        };

        let mut pktcnt: c_int = 1;

        // Safety: vmnet_read is safe when called with valid parameters.
        let status = unsafe { vmnet_read(self.interface, &raw mut packet, &raw mut pktcnt) };

        if !status.is_success() {
            if status == VmnetReturnT::BufferExhausted {
                // No packets available.
                return Ok(0);
            }
            return Err(NetError::io(std::io::Error::other(status.message())));
        }

        if pktcnt == 0 {
            return Ok(0);
        }

        Ok(packet.vm_pkt_size as usize)
    }

    /// Writes a packet to the vmnet interface.
    ///
    /// # Errors
    ///
    /// Returns an error if the write operation fails.
    pub fn write_packet(&self, data: &[u8]) -> Result<usize> {
        if !self.is_running() {
            return Err(NetError::config("interface not running".to_string()));
        }

        if data.len() > self.max_packet_size {
            return Err(NetError::config(format!(
                "packet too large: {} > {}",
                data.len(),
                self.max_packet_size
            )));
        }

        // Create iovec for the data.
        // Note: vmnet_write expects mutable pointers even though it doesn't modify the data.
        let mut iov = iovec {
            // vmnet_write expects *mut even though it treats the buffer as read-only
            iov_base: data.as_ptr().cast_mut().cast::<libc::c_void>(),
            iov_len: data.len(),
        };

        // Create packet descriptor.
        let mut packet = VmnetPacket {
            vm_pkt_iov: &raw mut iov,
            vm_pkt_iovcnt: 1,
            vm_pkt_size: data.len() as u32,
            vm_flags: 0,
        };

        let mut pktcnt: c_int = 1;

        // Safety: vmnet_write is safe when called with valid parameters.
        let status = unsafe { vmnet_write(self.interface, &raw mut packet, &raw mut pktcnt) };

        if !status.is_success() {
            return Err(NetError::io(std::io::Error::other(status.message())));
        }

        if pktcnt == 0 {
            return Err(NetError::io(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "no packets written",
            )));
        }

        Ok(data.len())
    }

    /// Stops the vmnet interface.
    pub fn stop(&self) {
        if self.running.swap(false, Ordering::AcqRel) {
            // Safety: vmnet_stop_interface is safe when called with valid parameters.
            unsafe {
                vmnet_stop_interface(self.interface, self.queue, ptr::null());
            }
        }
    }
}

impl NetworkBackend for Vmnet {
    fn send(&self, data: &[u8]) -> Result<usize> {
        self.write_packet(data)
    }

    fn recv(&self, buf: &mut [u8]) -> Result<usize> {
        self.read_packet(buf)
    }

    fn mac(&self) -> [u8; 6] {
        self.mac
    }

    fn mtu(&self) -> u16 {
        self.mtu
    }
}

impl Drop for Vmnet {
    fn drop(&mut self) {
        self.stop();

        // Release the dispatch queue.
        // Safety: dispatch_release is safe for a valid queue.
        unsafe {
            dispatch_release(self.queue.cast());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn test_vmnet_config_default() {
        let config = VmnetConfig::default();
        assert_eq!(config.mode, VmnetMode::Shared);
        assert_eq!(config.mtu, DEFAULT_MTU);
        assert!(config.mac.is_none());
        assert!(config.bridge_interface.is_none());
        assert!(config.dhcp_start.is_none());
        assert!(config.dhcp_end.is_none());
        assert!(config.subnet_mask.is_none());
        assert!(!config.isolated);
    }

    #[test]
    fn test_vmnet_config_builders() {
        let config = VmnetConfig::shared()
            .with_mac([0x02, 0x00, 0x00, 0x00, 0x00, 0x01])
            .with_mtu(9000);

        assert_eq!(config.mode, VmnetMode::Shared);
        assert_eq!(config.mac, Some([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]));
        assert_eq!(config.mtu, 9000);

        let config = VmnetConfig::host_only().with_isolation(true);
        assert_eq!(config.mode, VmnetMode::HostOnly);
        assert!(config.isolated);

        let config = VmnetConfig::bridged("en0");
        assert_eq!(config.mode, VmnetMode::Bridged);
        assert_eq!(config.bridge_interface, Some("en0".to_string()));
    }

    #[test]
    fn test_vmnet_config_dhcp_range() {
        let config = VmnetConfig::shared()
            .with_dhcp_range(
                Ipv4Addr::new(192, 168, 64, 2),
                Ipv4Addr::new(192, 168, 64, 254),
            )
            .with_subnet_mask(Ipv4Addr::new(255, 255, 255, 0));

        assert_eq!(config.dhcp_start, Some(Ipv4Addr::new(192, 168, 64, 2)));
        assert_eq!(config.dhcp_end, Some(Ipv4Addr::new(192, 168, 64, 254)));
        assert_eq!(config.subnet_mask, Some(Ipv4Addr::new(255, 255, 255, 0)));
    }

    #[test]
    fn test_vmnet_config_jumbo_frame() {
        let config = VmnetConfig::shared().with_mtu(9000);
        assert_eq!(config.mtu, 9000);
    }

    #[test]
    fn test_vmnet_mode_conversion() {
        assert_eq!(
            VmnetOperatingMode::from(VmnetMode::Shared),
            VmnetOperatingMode::Shared
        );
        assert_eq!(
            VmnetOperatingMode::from(VmnetMode::HostOnly),
            VmnetOperatingMode::Host
        );
        assert_eq!(
            VmnetOperatingMode::from(VmnetMode::Bridged),
            VmnetOperatingMode::Bridged
        );
    }

    #[test]
    fn test_vmnet_mode_default() {
        assert_eq!(VmnetMode::default(), VmnetMode::Shared);
    }

    #[test]
    fn test_vmnet_config_clone() {
        let config = VmnetConfig::shared()
            .with_mac([0x02, 0x00, 0x00, 0x00, 0x00, 0x01])
            .with_mtu(1500);
        let cloned = config.clone();

        assert_eq!(config.mode, cloned.mode);
        assert_eq!(config.mac, cloned.mac);
        assert_eq!(config.mtu, cloned.mtu);
    }

    // Integration tests that require root/entitlements.
    // Run with: sudo cargo test -p arcbox-net vmnet_integration -- --ignored
    #[test]
    #[ignore]
    fn test_vmnet_create_shared() {
        let vmnet = Vmnet::new_shared();
        assert!(
            vmnet.is_ok(),
            "Failed to create shared vmnet: {:?}",
            vmnet.err()
        );

        let vmnet = vmnet.unwrap();
        assert!(vmnet.is_running());
        assert!(vmnet.max_packet_size() > 0);
        assert_eq!(vmnet.mtu(), DEFAULT_MTU);
    }

    #[test]
    #[ignore]
    fn test_vmnet_create_host_only() {
        let vmnet = Vmnet::new_host_only();
        assert!(
            vmnet.is_ok(),
            "Failed to create host-only vmnet: {:?}",
            vmnet.err()
        );

        let vmnet = vmnet.unwrap();
        assert!(vmnet.is_running());
    }

    #[test]
    #[ignore]
    fn test_vmnet_stop() {
        let vmnet = Vmnet::new_shared().expect("Failed to create vmnet");
        assert!(vmnet.is_running());

        vmnet.stop();
        assert!(!vmnet.is_running());
    }

    #[test]
    #[ignore]
    fn test_vmnet_read_no_data() {
        let vmnet = Vmnet::new_shared().expect("Failed to create vmnet");
        let mut buf = [0u8; 1500];

        // Reading with no data should return 0 or BufferExhausted.
        let result = vmnet.read_packet(&mut buf);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
    }

    #[test]
    #[ignore]
    fn test_vmnet_write_packet() {
        let vmnet = Vmnet::new_shared().expect("Failed to create vmnet");

        // Create a minimal Ethernet frame (14 bytes header + payload).
        let mut packet = vec![0u8; 64];
        // Destination MAC (broadcast).
        packet[0..6].copy_from_slice(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
        // Source MAC.
        packet[6..12].copy_from_slice(&vmnet.mac());
        // EtherType (0x0800 = IPv4).
        packet[12..14].copy_from_slice(&[0x08, 0x00]);

        let result = vmnet.write_packet(&packet);
        assert!(result.is_ok(), "Failed to write packet: {:?}", result.err());
    }

    #[test]
    #[ignore]
    fn test_vmnet_custom_mac() {
        let mac = [0x02, 0x00, 0x00, 0x12, 0x34, 0x56];
        let config = VmnetConfig::shared().with_mac(mac);
        let vmnet = Vmnet::new(config).expect("Failed to create vmnet");

        assert_eq!(vmnet.mac(), mac);
    }
}
