//! VirtIO entropy device (virtio-rng).
//!
//! Provides random bytes to the guest via /dev/hwrng.
//! This is one of the simplest VirtIO devices — it has a single
//! request queue and no configuration space.

use crate::{VirtioDevice, VirtioDeviceId};

/// VirtIO entropy (RNG) device.
pub struct VirtioRng {
    /// Device features.
    features: u64,
    /// Whether the device has been activated.
    active: bool,
}

impl VirtioRng {
    /// VirtIO 1.0 feature.
    pub const FEATURE_VERSION_1: u64 = 1 << 32;

    /// Creates a new VirtIO entropy device.
    pub fn new() -> Self {
        Self {
            features: Self::FEATURE_VERSION_1,
            active: false,
        }
    }
}

impl Default for VirtioRng {
    fn default() -> Self {
        Self::new()
    }
}

impl VirtioDevice for VirtioRng {
    fn device_id(&self) -> VirtioDeviceId {
        VirtioDeviceId::Rng
    }

    fn features(&self) -> u64 {
        self.features
    }

    fn ack_features(&mut self, features: u64) {
        self.features &= features;
    }

    fn read_config(&self, _offset: u64, data: &mut [u8]) {
        // RNG has no config space
        for b in data.iter_mut() {
            *b = 0;
        }
    }

    fn write_config(&mut self, _offset: u64, _data: &[u8]) {
        // No config space
    }

    fn activate(&mut self) -> crate::Result<()> {
        self.active = true;
        tracing::info!("VirtIO RNG activated");
        Ok(())
    }

    fn reset(&mut self) {
        self.active = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rng_device_id() {
        let rng = VirtioRng::new();
        assert_eq!(rng.device_id(), VirtioDeviceId::Rng);
    }

    #[test]
    fn test_rng_default() {
        let rng = VirtioRng::default();
        assert_eq!(rng.device_id(), VirtioDeviceId::Rng);
        assert!(!rng.active);
    }

    #[test]
    fn test_rng_features() {
        let rng = VirtioRng::new();
        assert!(rng.features() & VirtioRng::FEATURE_VERSION_1 != 0);
    }

    #[test]
    fn test_rng_ack_features() {
        let mut rng = VirtioRng::new();
        let original = rng.features();
        rng.ack_features(VirtioRng::FEATURE_VERSION_1);
        assert_eq!(rng.features(), original & VirtioRng::FEATURE_VERSION_1);
    }

    #[test]
    fn test_rng_config_read() {
        let rng = VirtioRng::new();
        let mut data = [0xFFu8; 4];
        rng.read_config(0, &mut data);
        assert_eq!(data, [0, 0, 0, 0]);
    }

    #[test]
    fn test_rng_activate_and_reset() {
        let mut rng = VirtioRng::new();

        assert!(!rng.active);
        rng.activate().unwrap();
        assert!(rng.active);

        rng.reset();
        assert!(!rng.active);
    }
}
