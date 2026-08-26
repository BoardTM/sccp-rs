#!/bin/sh
set -eu

usage() {
	printf 'Usage: sudo %s [module-path [asterisk-pid]]\n' "$0" >&2
}

file_inode() {
	if inode=$(stat -Lc %i "$1" 2>/dev/null); then
		printf '%s\n' "$inode"
	else
		stat -f %i "$1"
	fi
}

if [ "$#" -gt 2 ]; then
	usage
	exit 2
fi

module_path=${1:-}
if [ -z "$module_path" ]; then
	for candidate in \
		/usr/lib64/asterisk/modules/chan_sccp2.so \
		/usr/lib/asterisk/modules/chan_sccp2.so; do
		if [ -f "$candidate" ]; then
			module_path=$candidate
			break
		fi
	done
fi
if [ -z "$module_path" ] || [ ! -f "$module_path" ]; then
	printf 'chan_sccp2 module file was not found; pass its installed path explicitly\n' >&2
	exit 2
fi
module_path=$(readlink -f "$module_path")

asterisk_pid=${2:-}
if [ -z "$asterisk_pid" ] && command -v systemctl >/dev/null 2>&1; then
	asterisk_pid=$(systemctl show --property MainPID --value asterisk.service 2>/dev/null || true)
fi
if { [ -z "$asterisk_pid" ] || [ "$asterisk_pid" = 0 ]; } \
	&& [ -r /run/asterisk/asterisk.pid ]; then
	asterisk_pid=$(sed -n '1p' /run/asterisk/asterisk.pid)
fi
if { [ -z "$asterisk_pid" ] || [ "$asterisk_pid" = 0 ]; } \
	&& command -v pgrep >/dev/null 2>&1; then
	asterisk_pid=$(pgrep -o -x asterisk 2>/dev/null || true)
fi
case "$asterisk_pid" in
'' | 0 | *[!0-9]*)
	printf 'a running Asterisk PID was not found; pass it explicitly\n' >&2
	exit 2
	;;
esac

proc_root=${SCCP_MODULE_PROC_ROOT:-/proc}
maps_path="$proc_root/$asterisk_pid/maps"
if [ ! -r "$maps_path" ]; then
	printf 'cannot read %s; run this check with sudo or pass the correct Asterisk PID\n' \
		"$maps_path" >&2
	exit 2
fi

disk_inode=$(file_inode "$module_path")
mapped_inodes=$(awk -v module="$module_path" '$6 == module { print $5 }' "$maps_path" \
	| sort -u)
if [ -z "$mapped_inodes" ]; then
	printf 'chan_sccp2 is not mapped in Asterisk PID %s\n' "$asterisk_pid" >&2
	exit 1
fi

stale=0
for mapped_inode in $mapped_inodes; do
	if [ "$mapped_inode" != "$disk_inode" ]; then
		stale=1
	fi
done
if awk -v module="$module_path" \
	'$6 == module && $7 == "(deleted)" { stale = 1 } END { exit stale ? 0 : 1 }' \
	"$maps_path"; then
	stale=1
fi

if [ "$stale" -ne 0 ]; then
	printf 'STALE: Asterisk PID %s maps %s inode(s) [%s], but disk inode is %s\n' \
		"$asterisk_pid" "$module_path" \
		"$(printf '%s' "$mapped_inodes" | tr '\n' ' ')" "$disk_inode" >&2
	printf '%s\n' \
		'A module unload/load changed lifecycle state but did not load the replacement binary.' \
		'Restart the Asterisk process, then run this check again.' >&2
	exit 1
fi

printf 'OK: Asterisk PID %s maps %s at disk inode %s\n' \
	"$asterisk_pid" "$module_path" "$disk_inode"
