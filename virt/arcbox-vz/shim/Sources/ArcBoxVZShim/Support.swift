// Host capability queries (mirrors src/ffi/mod.rs "System Queries").

import IOKit
import Virtualization

func vzSupported() -> Bool {
    VZVirtualMachine.isSupported
}

func vzMinCPUCount() -> UInt64 {
    UInt64(VZVirtualMachineConfiguration.minimumAllowedCPUCount)
}

func vzMaxCPUCount() -> UInt64 {
    UInt64(VZVirtualMachineConfiguration.maximumAllowedCPUCount)
}

func vzMinMemorySize() -> UInt64 {
    VZVirtualMachineConfiguration.minimumAllowedMemorySize
}

func vzMaxMemorySize() -> UInt64 {
    VZVirtualMachineConfiguration.maximumAllowedMemorySize
}

/// The console session as IOKit's registry root reports it: 0 unlocked,
/// 1 locked, 2 nobody logged in at the console. A VZ save creates a Secure
/// Enclave key, which the Secure Enclave refuses unless the console user's
/// session is unlocked.
func consoleSessionState() -> Int32 {
    let root = IORegistryGetRootEntry(kIOMainPortDefault)
    defer { IOObjectRelease(root) }
    func property(_ key: String) -> Any? {
        IORegistryEntryCreateCFProperty(root, key as CFString, kCFAllocatorDefault, 0)?
            .takeRetainedValue()
    }
    guard let users = property("IOConsoleUsers") as? [Any], !users.isEmpty else { return 2 }
    return (property("IOConsoleLocked") as? Bool) == true ? 1 : 0
}
