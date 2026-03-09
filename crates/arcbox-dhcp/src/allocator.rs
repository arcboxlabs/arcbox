//! IP address pool allocator.
//!
//! Manages a contiguous range of IPv4 addresses, supporting sequential
//! allocation, specific-address allocation, and release.

use std::collections::HashSet;
use std::net::Ipv4Addr;

/// Simple IP address allocator.
///
/// Manages a pool of IP addresses within a subnet, allowing allocation
/// and release of addresses.
#[derive(Debug)]
pub struct IpAllocator {
    /// Start of the allocation range.
    start: u32,
    /// End of the allocation range (inclusive).
    end: u32,
    /// Set of allocated addresses.
    allocated: HashSet<u32>,
    /// Next address to try (for sequential allocation).
    next: u32,
}

impl IpAllocator {
    /// Creates a new IP allocator for the given range.
    ///
    /// # Panics
    ///
    /// Panics if start > end.
    #[must_use]
    pub fn new(start: Ipv4Addr, end: Ipv4Addr) -> Self {
        let start_u32 = u32::from(start);
        let end_u32 = u32::from(end);

        assert!(start_u32 <= end_u32, "start must be <= end");

        Self {
            start: start_u32,
            end: end_u32,
            allocated: HashSet::new(),
            next: start_u32,
        }
    }

    /// Allocates the next available IP address.
    ///
    /// Returns `None` if no addresses are available.
    pub fn allocate(&mut self) -> Option<Ipv4Addr> {
        // Try sequential allocation first (faster for sparse usage)
        let range_size = self.end - self.start + 1;

        for _ in 0..range_size {
            if !self.allocated.contains(&self.next) {
                let addr = self.next;
                self.allocated.insert(addr);
                self.next = if self.next >= self.end {
                    self.start
                } else {
                    self.next + 1
                };
                return Some(Ipv4Addr::from(addr));
            }

            self.next = if self.next >= self.end {
                self.start
            } else {
                self.next + 1
            };
        }

        None // All addresses allocated
    }

    /// Allocates a specific IP address.
    ///
    /// Returns `true` if the address was successfully allocated,
    /// `false` if it's already allocated or out of range.
    pub fn allocate_specific(&mut self, ip: Ipv4Addr) -> bool {
        let ip_u32 = u32::from(ip);

        if ip_u32 < self.start || ip_u32 > self.end {
            return false;
        }

        if self.allocated.contains(&ip_u32) {
            return false;
        }

        self.allocated.insert(ip_u32);
        true
    }

    /// Releases an IP address back to the pool.
    pub fn release(&mut self, ip: Ipv4Addr) {
        let ip_u32 = u32::from(ip);
        self.allocated.remove(&ip_u32);
    }

    /// Checks if an IP address is available.
    #[must_use]
    pub fn is_available(&self, ip: Ipv4Addr) -> bool {
        let ip_u32 = u32::from(ip);
        ip_u32 >= self.start && ip_u32 <= self.end && !self.allocated.contains(&ip_u32)
    }

    /// Returns the number of allocated addresses.
    #[must_use]
    pub fn allocated_count(&self) -> usize {
        self.allocated.len()
    }

    /// Returns the number of available addresses.
    #[must_use]
    pub fn available_count(&self) -> usize {
        let total = (self.end - self.start + 1) as usize;
        total - self.allocated.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sequential_allocation() {
        let start = Ipv4Addr::new(192, 168, 1, 10);
        let end = Ipv4Addr::new(192, 168, 1, 12);
        let mut allocator = IpAllocator::new(start, end);

        assert_eq!(allocator.allocate(), Some(Ipv4Addr::new(192, 168, 1, 10)));
        assert_eq!(allocator.allocate(), Some(Ipv4Addr::new(192, 168, 1, 11)));
        assert_eq!(allocator.allocate(), Some(Ipv4Addr::new(192, 168, 1, 12)));
        assert_eq!(allocator.allocate(), None);
    }

    #[test]
    fn test_allocate_specific() {
        let start = Ipv4Addr::new(192, 168, 1, 10);
        let end = Ipv4Addr::new(192, 168, 1, 20);
        let mut allocator = IpAllocator::new(start, end);

        assert!(allocator.allocate_specific(Ipv4Addr::new(192, 168, 1, 15)));
        assert!(!allocator.allocate_specific(Ipv4Addr::new(192, 168, 1, 15)));
        assert!(!allocator.allocate_specific(Ipv4Addr::new(192, 168, 1, 5)));
    }

    #[test]
    fn test_release_and_reuse() {
        let start = Ipv4Addr::new(192, 168, 1, 10);
        let end = Ipv4Addr::new(192, 168, 1, 10);
        let mut allocator = IpAllocator::new(start, end);

        let ip = allocator.allocate().unwrap();
        assert_eq!(allocator.allocate(), None);
        allocator.release(ip);
        assert_eq!(allocator.allocate(), Some(ip));
    }

    #[test]
    fn test_available_count() {
        let start = Ipv4Addr::new(10, 0, 0, 1);
        let end = Ipv4Addr::new(10, 0, 0, 10);
        let mut allocator = IpAllocator::new(start, end);

        assert_eq!(allocator.available_count(), 10);
        allocator.allocate();
        assert_eq!(allocator.available_count(), 9);
        assert_eq!(allocator.allocated_count(), 1);
    }
}
