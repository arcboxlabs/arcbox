#!/bin/sh
# Run the sidecar suite on ext4 and overlayfs with the same guest test binary.
set -eu

if [ "$#" -ne 1 ] || [ "$(id -u)" -ne 0 ]; then
    echo "Usage: sudo $0 /absolute/path/to/arcbox-agent-test-binary" >&2
    exit 1
fi

test_binary=$(realpath "$1")
suite=machine_export::vfs::sidecar
test_list=$("$test_binary" --list)
ignored_tests=$("$test_binary" --list --ignored)
require_test() {
    case "
$1
" in
        *"
$2: test
"*) ;;
        *) echo "The test binary does not contain the required test: $2" >&2; exit 1 ;;
    esac
}
require_test "$test_list" "$suite::xattrs::tests::a_side_entry_of_a_recreated_file_is_dropped"
require_test "$ignored_tests" "$suite::side_entry::mount_tests::mounted_roots_preserve_remounts_and_distinguish_filesystems"
require_test "$ignored_tests" "$suite::side_entry::mount_tests::overlay_copy_up_and_remount_preserve_the_side_entry"

run_tests() {
    if test_output=$("$test_binary" "$@" --nocapture 2>&1); then
        printf '%s\n' "$test_output"
    else
        status=$?
        printf '%s\n' "$test_output" >&2
        return "$status"
    fi
    case "
$test_output
" in
        *"
test result: ok. "[1-9]*" passed; 0 failed; "*) ;;
        *) echo "Expected passing tests for filter: $1" >&2; return 1 ;;
    esac
}

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
TMPDIR="$fixture/plain" run_tests "$suite"
TMPDIR="$fixture/plain" run_tests "$suite::side_entry::mount_tests" --ignored
mount -t overlay overlay -o "lowerdir=$fixture/lower,upperdir=$fixture/upper,workdir=$fixture/work" "$fixture/merged"
mounted=true
TMPDIR="$fixture/merged" run_tests "$suite"
