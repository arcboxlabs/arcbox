//! Test: Can GIC be created after mapping 2GB RAM at IPA 0?

fn main() {
    println!("Test: GIC creation with 2GB RAM mapped at IPA 0\n");

    let vm = arcbox_hv::HvVm::with_ipa_size(40).expect("VM create failed");

    // Map 2GB RAM at IPA 0 (same as darwin_hv.rs)
    let ram_size = 2 * 1024 * 1024 * 1024usize;
    let layout = std::alloc::Layout::from_size_align(ram_size, 4096).unwrap();
    let ram_ptr = unsafe { std::alloc::alloc_zeroed(layout) };
    assert!(!ram_ptr.is_null(), "failed to allocate 2GB");

    unsafe {
        vm.map_memory(ram_ptr, 0, ram_size, arcbox_hv::MemoryPermission::ALL)
            .expect("map_memory failed");
    }
    println!("Mapped 2GB RAM at IPA 0x0..0x{:x}", ram_size);

    // Now try GIC creation
    let config = arcbox_hv::GicConfig {
        distributor_base: 0x0800_0000,
        redistributor_base: 0x080A_0000,
    };
    match arcbox_hv::Gic::new(config) {
        Ok(gic) => {
            println!("GIC created OK with GICD=0x08000000 inside RAM range!");
            println!("  distributor_base = {:#x}", gic.distributor_base());
        }
        Err(e) => {
            println!("GIC FAILED: {e}");
            println!();
            println!("RAM covers IPA 0x0..0x80000000, GICD at 0x08000000 is INSIDE this range.");
            println!("The GIC base must be OUTSIDE the RAM mapping.");
            println!();

            // Try with GIC above RAM
            println!("Retrying with GICD above RAM...");
            vm.unmap_memory(0, ram_size).expect("unmap failed");
            // Map RAM at 0x4000_0000 instead (standard ARM64 layout)
            unsafe {
                vm.map_memory(
                    ram_ptr,
                    0x4000_0000,
                    ram_size,
                    arcbox_hv::MemoryPermission::ALL,
                )
                .expect("remap failed");
            }
            println!("Remapped RAM at IPA 0x40000000..0xC0000000");

            let config2 = arcbox_hv::GicConfig {
                distributor_base: 0x0800_0000,
                redistributor_base: 0x080A_0000,
            };
            match arcbox_hv::Gic::new(config2) {
                Ok(_) => println!("GIC created OK with RAM at 0x40000000!"),
                Err(e2) => println!("GIC still failed: {e2}"),
            }
        }
    }

    unsafe { std::alloc::dealloc(ram_ptr, layout) };
    drop(vm);
    println!("\nDone.");
}
