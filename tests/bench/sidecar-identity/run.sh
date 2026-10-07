#!/bin/sh
# Run the sidecar suite on ext4 and overlayfs with the same guest test binary.
set -eu

if [ "$#" -ne 1 ] || [ "$(id -u)" -ne 0 ]; then
    echo "Usage: sudo $0 /absolute/path/to/arcbox-agent-test-binary" >&2
    exit 1
fi

fixture=$(mktemp -d /tmp/arcbox-sidecar-identity.XXXXXXXX)
mounted=false
cleanup() {
    if [ "$mounted" = true ]; then
        umount "$fixture/merged"
    fi
    rm -rf "$fixture"
}
trap cleanup EXIT

if [ "$(findmnt -n -o FSTYPE --target "$fixture")" != ext4 ]; then
    echo "The /tmp filesystem must be ext4 for this probe." >&2
    exit 1
fi
mkdir "$fixture/plain" "$fixture/lower" "$fixture/upper" "$fixture/work" "$fixture/merged"
TMPDIR="$fixture/plain" "$1" machine_export::vfs::sidecar --nocapture
TMPDIR="$fixture/plain" "$1" machine_export::vfs::sidecar::side_entry::mount_tests --ignored --nocapture
mount -t overlay overlay -o "lowerdir=$fixture/lower,upperdir=$fixture/upper,workdir=$fixture/work" "$fixture/merged"
mounted=true
TMPDIR="$fixture/merged" "$1" machine_export::vfs::sidecar --nocapture
