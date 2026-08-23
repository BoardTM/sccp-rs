#!/bin/sh
set -eu

WARMUP_CYCLES=${SCCP_LIFECYCLE_WARMUP_CYCLES:-4}
BATCH_CYCLES=${SCCP_LIFECYCLE_BATCH_CYCLES:-12}
RSS_TOLERANCE_KB=${SCCP_LIFECYCLE_RSS_TOLERANCE_KB:-1024}
SCCP_PORT=${SCCP_LIFECYCLE_PORT:-24999}
LIVE_BRIDGES=${SCCP_LIVE_BRIDGES:-0}

module_path=${1:-}
native_module_dir=${ASTERISK_MODULE_DIR:-/usr/lib/asterisk/modules}
native_data_dir=${ASTERISK_DATA_DIR:-/var/lib/asterisk}
asterisk_bin=${ASTERISK_BIN:-asterisk}

for bounded_value in "$WARMUP_CYCLES" "$BATCH_CYCLES" "$RSS_TOLERANCE_KB" "$SCCP_PORT"; do
	case "$bounded_value" in
	'' | *[!0-9]*)
		printf 'lifecycle bounds and port must be unsigned decimal integers\n' >&2
		exit 2
		;;
	esac
done
if [ "$LIVE_BRIDGES" != 0 ] && [ "$LIVE_BRIDGES" != 1 ]; then
	printf 'SCCP_LIVE_BRIDGES must be 0 or 1\n' >&2
	exit 2
fi
if [ "$WARMUP_CYCLES" -lt 1 ] || [ "$WARMUP_CYCLES" -gt 20 ] \
	|| [ "$BATCH_CYCLES" -lt 1 ] || [ "$BATCH_CYCLES" -gt 50 ] \
	|| [ "$RSS_TOLERANCE_KB" -gt 16384 ] \
	|| [ "$SCCP_PORT" -lt 1 ] || [ "$SCCP_PORT" -gt 65535 ]; then
	printf 'lifecycle bounds or port are outside the permitted range\n' >&2
	exit 2
fi

if [ -z "$module_path" ] || [ ! -f "$module_path" ]; then
	printf 'usage: %s /path/to/libchan_sccp2.so\n' "$0" >&2
	exit 2
fi
if [ ! -d "$native_module_dir" ]; then
	printf 'Asterisk module directory does not exist: %s\n' "$native_module_dir" >&2
	exit 2
fi
if [ ! -d "$native_data_dir/documentation" ]; then
	printf 'Asterisk documentation directory does not exist: %s/documentation\n' \
		"$native_data_dir" >&2
	exit 2
fi
if ! command -v "$asterisk_bin" >/dev/null 2>&1; then
	printf 'Asterisk executable is unavailable: %s\n' "$asterisk_bin" >&2
	exit 2
fi

test_root=$(mktemp -d /tmp/chan-sccp2-lifecycle.XXXXXX)
asterisk_pid=
diagnostics="$test_root/lifecycle.tsv"
asterisk_log="$test_root/asterisk.log"
cli_log="$test_root/cli.log"

finish() {
	status=$1
	trap - EXIT HUP INT TERM
	if [ -n "$asterisk_pid" ] && kill -0 "$asterisk_pid" 2>/dev/null; then
		kill "$asterisk_pid" 2>/dev/null || true
		wait "$asterisk_pid" 2>/dev/null || true
	fi
	if [ "$status" -ne 0 ]; then
		printf '\nNative lifecycle diagnostics:\n' >&2
		if [ -f "$diagnostics" ]; then
			cat "$diagnostics" >&2
		fi
		printf '\nAsterisk CLI transcript:\n' >&2
		if [ -f "$cli_log" ]; then
			cat "$cli_log" >&2
		fi
		printf '\nAsterisk log:\n' >&2
		if [ -f "$asterisk_log" ]; then
			cat "$asterisk_log" >&2
		fi
	fi
	rm -rf "$test_root"
	exit "$status"
}
trap 'finish $?' EXIT
trap 'exit 130' HUP INT TERM

mkdir -p \
	"$test_root/etc" \
	"$test_root/modules" \
	"$test_root/var/lib" \
	"$test_root/var/lib/moh" \
	"$test_root/var/db" \
	"$test_root/var/key" \
	"$test_root/var/spool" \
	"$test_root/var/run" \
	"$test_root/var/log"

if [ -d /etc/asterisk ]; then
	cp -R /etc/asterisk/. "$test_root/etc/"
fi
for installed_module in "$native_module_dir"/*.so; do
	if [ -f "$installed_module" ]; then
		ln -s "$installed_module" "$test_root/modules/$(basename "$installed_module")"
	fi
done
cp "$module_path" "$test_root/modules/chan_sccp2.so"

cat >"$test_root/etc/asterisk.conf" <<EOF
[directories]
astetcdir => $test_root/etc
astmoddir => $test_root/modules
astvarlibdir => $test_root/var/lib
astdbdir => $test_root/var/db
astkeydir => $test_root/var/key
astdatadir => $native_data_dir
astagidir => $test_root/var/lib/agi-bin
astspooldir => $test_root/var/spool
astrundir => $test_root/var/run
astlogdir => $test_root/var/log
astsbindir => /usr/sbin

[options]
verbose = 0
debug = 0
nocolor = yes
documentation_language = en_US
EOF

cat >"$test_root/etc/modules.conf" <<'EOF'
[modules]
autoload = no
noload = chan_sccp2.so
EOF

if [ "$LIVE_BRIDGES" -eq 1 ]; then
	for required_module in \
		res_timing_timerfd.so bridge_simple.so bridge_softmix.so codec_ulaw.so \
		format_pcm.so res_musiconhold.so app_confbridge.so; do
		if [ ! -f "$test_root/modules/$required_module" ]; then
			printf 'live bridge dependency is unavailable: %s\n' "$required_module" >&2
			exit 2
		fi
		printf 'load = %s\n' "$required_module" >>"$test_root/etc/modules.conf"
	done
	dd if=/dev/zero of="$test_root/var/lib/moh/silence.ulaw" \
		bs=160 count=50 2>/dev/null
	cat >"$test_root/etc/musiconhold.conf" <<EOF
[default]
mode = files
directory = $test_root/var/lib/moh
EOF
	cat >"$test_root/etc/confbridge.conf" <<'EOF'
[default_user]
type = user
music_on_hold_when_empty = no

[default_bridge]
type = bridge
EOF
fi

cat >"$test_root/etc/sccp.conf" <<EOF
[general]
bind = 127.0.0.1:$SCCP_PORT
advertised_address = 127.0.0.1
disallow = all
allow = ulaw

[SEP001122334455]
type = device
line = 1001

[1001]
type = line
label = Lifecycle
context = default
EOF

SCCP_CONFIG="$test_root/etc/sccp.conf" \
	"$asterisk_bin" -C "$test_root/etc/asterisk.conf" -f -g -q \
	>"$asterisk_log" 2>&1 &
asterisk_pid=$!

cli() {
	"$asterisk_bin" -C "$test_root/etc/asterisk.conf" -rx "$1"
}

capture_cli() {
	label=$1
	command=$2
	printf '\n[%s] %s\n' "$label" "$command" >>"$cli_log"
	if ! cli "$command" >>"$cli_log" 2>&1; then
		printf 'Asterisk CLI command failed during lifecycle cycle %s: %s\n' \
			"$label" "$command" >&2
		return 1
	fi
}

ready=0
attempt=0
while [ "$attempt" -lt 100 ]; do
	if ! kill -0 "$asterisk_pid" 2>/dev/null; then
		printf 'Asterisk exited during startup\n' >&2
		exit 1
	fi
	if cli 'core show uptime' >/dev/null 2>&1; then
		ready=1
		break
	fi
	attempt=$((attempt + 1))
	sleep 0.1
done
if [ "$ready" -ne 1 ]; then
	printf 'Asterisk did not become ready within 10 seconds\n' >&2
	exit 1
fi

count_running_module_rows() {
	awk '$1 == "chan_sccp2.so" && $(NF - 2) != "Not" && $(NF - 1) == "Running" { count += 1 } \
		END { print count + 0 }'
}

running_module_count() {
	cli 'module show like chan_sccp2.so' | count_running_module_rows
}

channel_driver_count() {
	cli 'core show channeltypes' \
		| awk '$1 == "SCCP" { count += 1 } END { print count + 0 }'
}

module_status_fixture='chan_sccp2.so Rust SCCP Channel Driver 0 Running extended
chan_sccp2.so Rust SCCP Channel Driver 0 Not Running extended'
if [ "$(printf '%s\n' "$module_status_fixture" | count_running_module_rows)" -ne 1 ]; then
	printf 'module status parser did not distinguish Running from Not Running\n' >&2
	exit 1
fi

assert_alive() {
	if ! kill -0 "$asterisk_pid" 2>/dev/null; then
		printf 'Asterisk exited during lifecycle cycle %s\n' "$1" >&2
		exit 1
	fi
}

run_cycle() {
	cycle_label=$1
	assert_alive "$cycle_label"
	record_metrics "$cycle_label-start"
	capture_cli "$cycle_label-load" 'module load chan_sccp2.so'
	capture_cli "$cycle_label-loaded-module" 'module show like chan_sccp2.so'
	capture_cli "$cycle_label-loaded-channeltypes" 'core show channeltypes'
	if [ "$(running_module_count)" -ne 1 ] || [ "$(channel_driver_count)" -ne 1 ]; then
		printf 'module was not running during lifecycle cycle %s\n' "$cycle_label" >&2
		exit 1
	fi
	if [ "$LIVE_BRIDGES" -eq 1 ]; then
		bridge_result=$(cli 'sccp test bridges')
		printf '\n[%s-bridges] sccp test bridges\n%s\n' \
			"$cycle_label" "$bridge_result" >>"$cli_log"
		case "$bridge_result" in
		*'CONF-020 PASS scenarios=10'*) ;;
		*)
			printf 'live bridge harness failed during lifecycle cycle %s\n' \
				"$cycle_label" >&2
			exit 1
			;;
		esac
	fi
	capture_cli "$cycle_label-unload" 'module unload chan_sccp2.so'
	capture_cli "$cycle_label-unloaded-module" 'module show like chan_sccp2.so'
	capture_cli "$cycle_label-unloaded-channeltypes" 'core show channeltypes'
	if [ "$(running_module_count)" -ne 0 ] || [ "$(channel_driver_count)" -ne 0 ]; then
		printf 'module remained running after lifecycle cycle %s\n' "$cycle_label" >&2
		exit 1
	fi
	assert_alive "$cycle_label"
	record_metrics "$cycle_label-end"
}

metric() {
	case "$1" in
	fd)
		find "/proc/$asterisk_pid/fd" -mindepth 1 -maxdepth 1 -print | wc -l | awk '{ print $1 }'
		;;
	threads)
		find "/proc/$asterisk_pid/task" -mindepth 1 -maxdepth 1 -print | wc -l | awk '{ print $1 }'
		;;
	rss)
		awk '/^VmRSS:/ { print $2 }' "/proc/$asterisk_pid/status"
		;;
	*)
		return 2
		;;
	esac
}

record_metrics() {
	label=$1
	fd_count=$(metric fd)
	thread_count=$(metric threads)
	rss_kb=$(metric rss)
	printf '%s\t%s\t%s\t%s\n' "$label" "$fd_count" "$thread_count" "$rss_kb" >>"$diagnostics"
}

printf 'step\tfds\tthreads\trss_kb\n' >"$diagnostics"
cycle=1
while [ "$cycle" -le "$WARMUP_CYCLES" ]; do
	run_cycle "warmup-$cycle"
	cycle=$((cycle + 1))
done
record_metrics warmup
baseline_fds=$(metric fd)
baseline_threads=$(metric threads)

batch=1
while [ "$batch" -le 3 ]; do
	cycle=1
	while [ "$cycle" -le "$BATCH_CYCLES" ]; do
		run_cycle "batch-$batch-$cycle"
		cycle=$((cycle + 1))
	done
	record_metrics "batch-$batch"
	if [ "$(metric fd)" -ne "$baseline_fds" ]; then
		printf 'file descriptor count changed after batch %s\n' "$batch" >&2
		exit 1
	fi
	if [ "$(metric threads)" -ne "$baseline_threads" ]; then
		printf 'thread count changed after batch %s\n' "$batch" >&2
		exit 1
	fi
	if [ "$batch" -eq 2 ]; then
		second_batch_rss=$(metric rss)
	fi
	batch=$((batch + 1))
done

final_rss=$(metric rss)
maximum_final_rss=$((second_batch_rss + RSS_TOLERANCE_KB))
if [ "$final_rss" -gt "$maximum_final_rss" ]; then
	printf 'RSS grew from %s KiB after batch 2 to %s KiB after batch 3 (limit +%s KiB)\n' \
		"$second_batch_rss" "$final_rss" "$RSS_TOLERANCE_KB" >&2
	exit 1
fi

cli 'core stop now' >/dev/null
wait "$asterisk_pid"
asterisk_pid=
printf 'Native lifecycle gate passed: %s warmup + %s measured load/unload cycles\n' \
	"$WARMUP_CYCLES" "$((BATCH_CYCLES * 3))"
if [ "$LIVE_BRIDGES" -eq 1 ]; then
	printf 'Native bridge gate passed across every lifecycle cycle\n'
fi
