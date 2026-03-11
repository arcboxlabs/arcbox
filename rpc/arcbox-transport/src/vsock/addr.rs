/// Default port for `ArcBox` agent communication.
pub use arcbox_constants::ports::AGENT_PORT as DEFAULT_AGENT_PORT;

/// Vsock address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VsockAddr {
    /// Context ID.
    pub cid: u32,
    /// Port number.
    pub port: u32,
}

impl VsockAddr {
    /// Hypervisor CID (reserved).
    pub const CID_HYPERVISOR: u32 = 0;
    /// Local CID.
    pub const CID_LOCAL: u32 = 1;
    /// Host CID (from guest perspective).
    pub const CID_HOST: u32 = 2;
    /// Any CID (for binding).
    pub const CID_ANY: u32 = u32::MAX;

    /// Creates a new vsock address.
    #[must_use]
    pub const fn new(cid: u32, port: u32) -> Self {
        Self { cid, port }
    }

    /// Creates an address for the host (from guest perspective).
    #[must_use]
    pub const fn host(port: u32) -> Self {
        Self::new(Self::CID_HOST, port)
    }

    /// Creates an address for any CID (for binding).
    #[must_use]
    pub const fn any(port: u32) -> Self {
        Self::new(Self::CID_ANY, port)
    }
}
