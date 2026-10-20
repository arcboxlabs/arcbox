#!/usr/bin/env bash
# Run only on Linux. All block devices below belong to this temporary fixture.
set -euo pipefail

if [[ ${EUID} != 0 || $# != 2 ]]; then
  echo "Run as root: $0 <Linux agent test binary> <recovery rootfs.erofs>" >&2
  exit 1
fi
test_binary=$(realpath "$1")
rootfs=$(realpath "$2")
storage_tests=(
  agent::linux::storage_check::linux_integration::clean_offline_checks_preserve_both_images
  agent::linux::storage_check::linux_integration::mounted_volumes_pass_health_and_durable_write_probes
  agent::linux::storage_check::linux_integration::local_docker_probe_creates_runs_and_cleans_up
  agent::linux::storage_check::linux_integration::kernel_io_failure_forces_btrfs_read_only_and_health_reports_it
  agent::linux::storage_check::linux_integration::stale_free_space_tree_is_classified_repaired_and_rechecked
)
# btrfs-progs' own fixture for free space tree entries without a block group.
# It is GPL-licensed, so it is downloaded here and never stored in this repository.
stale_fst_url=https://raw.githubusercontent.com/kdave/btrfs-progs/v7.1/tests/fsck-tests/058-bad-free-space-tree-entry/corrupt-free-space-tree.img.xz
stale_fst_sha256=5a2c84efec37210f6dc00e86f1756e3358f431aebf44702c8f50acc7e011a87f
test_list=$("$test_binary" --list --ignored)
for test_name in "${storage_tests[@]}"; do
  if [[ $'\n'"$test_list"$'\n' != *$'\n'"$test_name: test"$'\n'* ]]; then
    echo "The test binary does not contain the required ignored test: $test_name" >&2
    exit 1
  fi
done

run_test() {
  local test_output status
  if test_output=$("$test_binary" "$1" --exact --ignored --nocapture 2>&1); then
    printf '%s\n' "$test_output"
  else
    status=$?
    printf '%s\n' "$test_output" >&2
    return "$status"
  fi
  if [[ $'\n'"$test_output" != *$'\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; '* ]]; then
    echo "Expected exactly one passing test: $1" >&2
    return 1
  fi
}

for program in dmsetup losetup mount umount mountpoint truncate blockdev dmesg curl xz btrfs-image sha256sum; do
  command -v "$program" >/dev/null
done

fixture=$(mktemp -d /tmp/arcbox-storage-test.XXXXXXXX)
data_loop=""
metadata_loop=""
stale_fst_loop=""
mapper=""
cleanup() {
  status=$?
  trap - EXIT
  set +e
  cleanup_status=0
  for directory in metadata data rootfs; do
    if mountpoint -q "$fixture/$directory"; then
      umount "$fixture/$directory" || cleanup_status=1
    fi
  done
  if [[ -n $mapper ]]; then dmsetup remove "$mapper" || cleanup_status=1; fi
  if [[ -n $stale_fst_loop ]]; then losetup -d "$stale_fst_loop" || cleanup_status=1; fi
  if [[ -n $metadata_loop ]]; then losetup -d "$metadata_loop" || cleanup_status=1; fi
  if [[ -n $data_loop ]]; then losetup -d "$data_loop" || cleanup_status=1; fi
  if [[ $cleanup_status == 0 ]]; then
    rm -rf "$fixture"
  else
    echo "Cleanup failed. Preserve fixture for inspection: $fixture" >&2
  fi
  if [[ $status != 0 ]]; then exit "$status"; fi
  exit "$cleanup_status"
}
trap cleanup EXIT

mkdir "$fixture/rootfs" "$fixture/data" "$fixture/metadata"
mount -t erofs -o loop,ro "$rootfs" "$fixture/rootfs"
tools="$fixture/rootfs/sbin"
truncate -s 256M "$fixture/data.img"
truncate -s 64M "$fixture/metadata.img"
data_loop=$(losetup --find --show "$fixture/data.img")
metadata_loop=$(losetup --find --show "$fixture/metadata.img")
mapper="arcbox-storage-test-$(cat /proc/sys/kernel/random/uuid)"
dmsetup create "$mapper" --table "0 $(blockdev --getsz "$data_loop") linear $data_loop 0"
data_device="/dev/mapper/$mapper"
"$tools/mkfs.btrfs" -f "$data_device"
"$tools/mkfs.ext4" -F "$metadata_loop"

export ARCBOX_STORAGE_TEST_DATA_IMAGE="$fixture/data.img"
export ARCBOX_STORAGE_TEST_METADATA_IMAGE="$fixture/metadata.img"
export ARCBOX_STORAGE_TEST_DATA_DEVICE="$data_device"
export ARCBOX_STORAGE_TEST_METADATA_DEVICE="$metadata_loop"
export ARCBOX_STORAGE_TEST_DATA_MOUNT="$fixture/data"
export ARCBOX_STORAGE_TEST_METADATA_MOUNT="$fixture/metadata"
export ARCBOX_STORAGE_TEST_TOOLS="$tools"
export ARCBOX_STORAGE_TEST_BUSYBOX="$fixture/rootfs/bin/busybox"
export ARCBOX_STORAGE_TEST_MAPPER="$mapper"

curl -fsSL --retry 5 -o "$fixture/stale-fst.img.xz" "$stale_fst_url"
echo "$stale_fst_sha256  $fixture/stale-fst.img.xz" | sha256sum -c -
xz -dc "$fixture/stale-fst.img.xz" > "$fixture/stale-fst.dump"
btrfs-image -r "$fixture/stale-fst.dump" "$fixture/stale-fst.img"
stale_fst_loop=$(losetup --find --show "$fixture/stale-fst.img")
export ARCBOX_STORAGE_TEST_STALE_FST_DEVICE="$stale_fst_loop"

run_test "${storage_tests[0]}"
mount -t btrfs -o noatime,nodiscard "$data_device" "$fixture/data"
mount -t ext4 "$metadata_loop" "$fixture/metadata"
run_test "${storage_tests[1]}"
run_test "${storage_tests[2]}"
run_test "${storage_tests[3]}"
run_test "${storage_tests[4]}"
