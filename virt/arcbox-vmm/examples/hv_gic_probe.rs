//! Probe GIC parameters from Hypervisor.framework.
//! Build: cargo build --example hv_gic_probe -p arcbox-vmm --features gic
//! Sign: codesign --force --options runtime --entitlements bundle/arcbox.entitlements \
//!   --sign "Developer ID Application: ArcBox, Inc. (422ACSY6Y5)" target/debug/examples/hv_gic_probe
//! Run: ./target/debug/examples/hv_gic_probe

fn main() {
    println!("Probing Hypervisor.framework GIC parameters...\n");

    // Create VM first (required before GIC operations).
    let vm = arcbox_hv::HvVm::with_ipa_size(40).expect("hv_vm_create failed");

    // Query alignment and size requirements BEFORE creating GIC.
    unsafe {
        let mut val: usize = 0;

        let r = arcbox_hv_ffi::hv_gic_get_distributor_size(&raw mut val);
        println!("hv_gic_get_distributor_size: ret={r}, val={val:#x} ({val} bytes)");

        let r = arcbox_hv_ffi::hv_gic_get_distributor_base_alignment(&raw mut val);
        println!("hv_gic_get_distributor_base_alignment: ret={r}, val={val:#x}");

        let r = arcbox_hv_ffi::hv_gic_get_redistributor_region_size(&raw mut val);
        println!("hv_gic_get_redistributor_region_size: ret={r}, val={val:#x} ({val} bytes)");

        let r = arcbox_hv_ffi::hv_gic_get_redistributor_size(&raw mut val);
        println!("hv_gic_get_redistributor_size: ret={r}, val={val:#x}");

        let mut base: u32 = 0;
        let mut count: u32 = 0;
        let r = arcbox_hv_ffi::hv_gic_get_spi_interrupt_range(&raw mut base, &raw mut count);
        println!("hv_gic_get_spi_interrupt_range: ret={r}, base={base}, count={count}");
    }

    // Try creating GIC with various base addresses.
    println!("\nTrying GIC creation with different bases:");

    for (gicd, gicr) in [
        (0x0800_0000u64, 0x080A_0000u64),
        (0x0800_0000, 0x0810_0000),
        (0x0800_0000, 0x0900_0000),
        (0x4000_0000, 0x4010_0000),
    ] {
        let config = arcbox_hv::GicConfig {
            distributor_base: gicd,
            redistributor_base: gicr,
        };
        match arcbox_hv::Gic::new(config) {
            Ok(gic) => {
                println!("  GICD={gicd:#010x} GICR={gicr:#010x} -> OK");
                if let Ok(size) = gic.get_distributor_size() {
                    println!("    distributor_size={size:#x}");
                }
                if let Ok(size) = gic.get_redistributor_region_size() {
                    println!("    redistributor_region_size={size:#x}");
                }
                // GIC exists; we can't create another. Break.
                break;
            }
            Err(e) => {
                println!("  GICD={gicd:#010x} GICR={gicr:#010x} -> FAILED: {e}");
            }
        }
    }

    drop(vm);
    println!("\nDone.");
}

// Direct FFI access for probing (before GIC creation).
mod arcbox_hv_ffi {
    use std::ffi::c_void;
    type hv_return_t = i32;

    #[link(name = "Hypervisor", kind = "framework")]
    unsafe extern "C" {
        pub fn hv_gic_get_distributor_size(size: *mut usize) -> hv_return_t;
        pub fn hv_gic_get_distributor_base_alignment(alignment: *mut usize) -> hv_return_t;
        pub fn hv_gic_get_redistributor_region_size(size: *mut usize) -> hv_return_t;
        pub fn hv_gic_get_redistributor_size(size: *mut usize) -> hv_return_t;
        pub fn hv_gic_get_redistributor_base_alignment(alignment: *mut usize) -> hv_return_t;
        pub fn hv_gic_get_msi_region_size(size: *mut usize) -> hv_return_t;
        pub fn hv_gic_get_msi_region_base_alignment(alignment: *mut usize) -> hv_return_t;
        pub fn hv_gic_get_spi_interrupt_range(base: *mut u32, count: *mut u32) -> hv_return_t;
    }
}
