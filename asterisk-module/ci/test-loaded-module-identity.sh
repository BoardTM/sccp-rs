#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
verifier="$script_dir/../verify-loaded-module.sh"
test_root=$(mktemp -d /tmp/chan-sccp2-loaded-identity.XXXXXX)

cleanup() {
	rm -rf "$test_root"
}
trap cleanup EXIT HUP INT TERM

file_inode() {
	if inode=$(stat -Lc %i "$1" 2>/dev/null); then
		printf '%s\n' "$inode"
	else
		stat -f %i "$1"
	fi
}

module_path="$test_root/chan_sccp2.so"
asterisk_pid=4242
maps_dir="$test_root/proc/$asterisk_pid"
mkdir -p "$maps_dir"
printf 'first module image\n' >"$module_path"
module_path=$(readlink -f "$module_path")
initial_inode=$(file_inode "$module_path")
printf '00000000-00001000 r-xp 00000000 fe:01 %s %s\n' \
	"$initial_inode" "$module_path" >"$maps_dir/maps"

SCCP_MODULE_PROC_ROOT="$test_root/proc" \
	"$verifier" "$module_path" "$asterisk_pid" >"$test_root/current.out"
grep -Fq 'OK:' "$test_root/current.out"

printf 'replacement module image\n' >"$module_path.replacement"
mv "$module_path.replacement" "$module_path"
replacement_inode=$(file_inode "$module_path")
if [ "$replacement_inode" = "$initial_inode" ]; then
	printf 'fixture unexpectedly reused the original inode\n' >&2
	exit 1
fi
printf '00000000-00001000 r-xp 00000000 fe:01 %s %s (deleted)\n' \
	"$initial_inode" "$module_path" >"$maps_dir/maps"

if SCCP_MODULE_PROC_ROOT="$test_root/proc" \
	"$verifier" "$module_path" "$asterisk_pid" \
	>"$test_root/stale.out" 2>"$test_root/stale.err"; then
	printf 'verifier accepted a stale deleted module mapping\n' >&2
	exit 1
fi
grep -Fq 'STALE:' "$test_root/stale.err"
grep -Fq 'Restart the Asterisk process' "$test_root/stale.err"

printf '00000000-00001000 r-xp 00000000 fe:01 %s %s\n' \
	"$replacement_inode" "$module_path" >"$maps_dir/maps"
SCCP_MODULE_PROC_ROOT="$test_root/proc" \
	"$verifier" "$module_path" "$asterisk_pid" >"$test_root/restarted.out"
grep -Fq 'OK:' "$test_root/restarted.out"

printf 'Loaded-module identity verifier tests passed\n'
