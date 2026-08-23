#!/bin/sh
set -eu

backend=${1:-}
module_path=${2:-}
realtime_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
asterisk_bin=${ASTERISK_BIN:-/opt/asterisk-live/sbin/asterisk}
asterisk_module_dir=${ASTERISK_MODULE_DIR:-/opt/asterisk-live/lib/asterisk/modules}
asterisk_data_dir=${ASTERISK_DATA_DIR:-/opt/asterisk-live/var/lib/asterisk}

case "$backend" in
sqlite | postgresql | mysql) ;;
*)
	printf 'usage: %s <sqlite|postgresql|mysql> /path/to/libchan_sccp2.so\n' "$0" >&2
	exit 2
	;;
esac

if [ -z "$module_path" ] || [ ! -f "$module_path" ]; then
	printf 'module does not exist: %s\n' "$module_path" >&2
	exit 2
fi

test_root=$(mktemp -d "/tmp/chan-sccp2-realtime-${backend}.XXXXXX")
asterisk_pid=
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
		printf '\nAsterisk CLI transcript:\n' >&2
		[ ! -f "$cli_log" ] || cat "$cli_log" >&2
		printf '\nAsterisk log:\n' >&2
		[ ! -f "$asterisk_log" ] || cat "$asterisk_log" >&2
	fi
	rm -rf "$test_root"
	exit "$status"
}
trap 'finish $?' EXIT
trap 'exit 130' HUP INT TERM

mkdir -p \
	"$test_root/etc" \
	"$test_root/modules" \
	"$test_root/var/db" \
	"$test_root/var/key" \
	"$test_root/var/lib" \
	"$test_root/var/log" \
	"$test_root/var/run" \
	"$test_root/var/spool"

for installed_module in "$asterisk_module_dir"/*.so; do
	[ ! -f "$installed_module" ] || \
		ln -s "$installed_module" "$test_root/modules/$(basename "$installed_module")"
done
cp "$module_path" "$test_root/modules/chan_sccp2.so"

database_exec() {
	case "$backend" in
	sqlite)
		sqlite3 -batch "$test_root/realtime.sqlite3"
		;;
	postgresql)
		psql --username=root --dbname=sccp2_realtime --quiet --set ON_ERROR_STOP=1
		;;
	mysql)
		mysql --user=root --database=sccp2_realtime \
			--default-character-set=utf8mb4
		;;
	esac
}

database_exec <"$realtime_dir/$backend/001_initial.up.sql"
database_exec <"$realtime_dir/provider-fixtures.sql"

case "$backend" in
sqlite)
	realtime_module=res_config_sqlite3.so
	cat >"$test_root/etc/res_config_sqlite3.conf" <<EOF
[sccp]
dbfile => $test_root/realtime.sqlite3
batch => 0
requirements => warn
EOF
	driver=sqlite3
	database=sccp
	;;
postgresql)
	realtime_module=res_config_pgsql.so
	cat >"$test_root/etc/res_pgsql.conf" <<'EOF'
[general]
socket=/var/run/postgresql
dbname=sccp2_realtime
user=root
requirements=warn
order_multi_row_results_by_initial_column=yes
EOF
	driver=pgsql
	database=sccp2_realtime
	;;
mysql)
	realtime_module=res_config_mysql.so
	cat >"$test_root/etc/res_config_mysql.conf" <<'EOF'
[general]
dbhost=localhost
dbname=sccp2_realtime
dbuser=root
dbsock=/run/mysqld/mysqld.sock
dbcharset=utf8mb4
requirements=warn
EOF
	driver=mysql
	database=general
	;;
esac

if [ ! -f "$test_root/modules/$realtime_module" ]; then
	printf 'realtime module is unavailable: %s\n' "$realtime_module" >&2
	exit 2
fi

cat >"$test_root/etc/asterisk.conf" <<EOF
[directories]
astetcdir => $test_root/etc
astmoddir => $test_root/modules
astvarlibdir => $test_root/var/lib
astdbdir => $test_root/var/db
astkeydir => $test_root/var/key
astdatadir => $asterisk_data_dir
astspooldir => $test_root/var/spool
astrundir => $test_root/var/run
astlogdir => $test_root/var/log

[options]
verbose = 0
debug = 0
nocolor = yes
documentation_language = en_US
EOF

cat >"$test_root/etc/modules.conf" <<EOF
[modules]
autoload = no
load = $realtime_module
noload = chan_sccp2.so
EOF

cat >"$test_root/etc/extconfig.conf" <<EOF
[settings]
sccp_devices => $driver,$database,sccp_devices
sccp_lines => $driver,$database,sccp_lines
EOF

cat >"$test_root/etc/sccp.conf" <<'EOF'
[general]
bind = 127.0.0.1:24998
advertised_address = 127.0.0.1
disallow = all
allow = ulaw
devicetable = sccp_devices
linetable = sccp_lines
EOF

SCCP_CONFIG="$test_root/etc/sccp.conf" \
	"$asterisk_bin" -C "$test_root/etc/asterisk.conf" -f -g -vvv \
	>"$asterisk_log" 2>&1 &
asterisk_pid=$!

cli() {
	command=$1
	printf '\n%s\n' "$command" >>"$cli_log"
	if output=$("$asterisk_bin" -C "$test_root/etc/asterisk.conf" -rx "$command" 2>&1); then
		status=0
	else
		status=$?
	fi
	printf '%s\n' "$output" >>"$cli_log"
	printf '%s\n' "$output"
	return "$status"
}

ready=0
attempt=0
while [ "$attempt" -lt 100 ]; do
	if ! kill -0 "$asterisk_pid" 2>/dev/null; then
		if wait "$asterisk_pid"; then
			asterisk_status=0
		else
			asterisk_status=$?
		fi
		asterisk_pid=
		printf 'Asterisk exited during startup (status %s)\n' \
			"$asterisk_status" >&2
		exit 1
	fi
	if "$asterisk_bin" -C "$test_root/etc/asterisk.conf" -rx 'core show uptime' \
		>/dev/null 2>&1; then
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

assert_contains() {
	label=$1
	actual=$2
	expected=$3
	case "$actual" in
	*"$expected"*) ;;
	*)
		printf '%s did not contain %s\nactual:\n%s\n' "$label" "$expected" "$actual" >&2
		exit 1
		;;
	esac
}

assert_not_contains() {
	label=$1
	actual=$2
	unexpected=$3
	case "$actual" in
	*"$unexpected"*)
		printf '%s unexpectedly contained %s\nactual:\n%s\n' \
			"$label" "$unexpected" "$actual" >&2
		exit 1
		;;
	*) ;;
	esac
}

cli 'module load chan_sccp2.so' >/dev/null
initial_device=$(cli 'sccp show devices SEP000000000001')
initial_buttons=$(cli 'sccp show devices SEP000000000001 buttons')
initial_lines=$(cli 'sccp show lines')
assert_contains initial-device "$initial_device" 'Description: Ordered value'
assert_contains initial-buttons "$initial_buttons" '1'
assert_contains initial-buttons "$initial_buttons" 'line'
assert_contains initial-buttons "$initial_buttons" '2'
assert_contains initial-buttons "$initial_buttons" 'speed-dial'
assert_contains initial-buttons "$initial_buttons" 'Support'
assert_contains initial-lines "$initial_lines" '1000'
assert_contains initial-lines "$initial_lines" 'Reception'
assert_contains initial-lines "$initial_lines" 'from-database'

printf 'UPDATE sccp2_realtime_active_generation SET generation_id = 3 WHERE singleton = 1;\n' \
	| database_exec
refresh=$(cli 'sccp reload')
assert_contains refresh "$refresh" 'SCCP configuration reloaded'
refreshed_devices=$(cli 'sccp show devices')
refreshed_device=$(cli 'sccp show devices SEP000000000003')
refreshed_buttons=$(cli 'sccp show devices SEP000000000003 buttons')
refreshed_lines=$(cli 'sccp show lines')
assert_contains refreshed-devices "$refreshed_devices" 'SEP000000000003'
assert_not_contains refreshed-devices "$refreshed_devices" 'SEP000000000001'
assert_contains refreshed-device "$refreshed_device" 'Description: Desk å'
assert_contains refreshed-buttons "$refreshed_buttons" 'Operations'
assert_contains refreshed-lines "$refreshed_lines" '3000'
assert_contains refreshed-lines "$refreshed_lines" 'Complete å'

printf 'UPDATE sccp2_realtime_active_generation SET generation_id = 4 WHERE singleton = 1;\n' \
	| database_exec
malformed=$(cli 'sccp reload')
assert_contains malformed "$malformed" 'Reload failed:'
after_malformed_devices=$(cli 'sccp show devices')
after_malformed_lines=$(cli 'sccp show lines')
assert_contains malformed-live-device "$after_malformed_devices" 'SEP000000000003'
assert_not_contains malformed-rejected-device "$after_malformed_devices" 'SEP000000000004'
assert_contains malformed-live-line "$after_malformed_lines" 'Complete å'

printf 'UPDATE sccp2_realtime_active_generation SET generation_id = 3 WHERE singleton = 1;\n' \
	| database_exec
database_exec <"$realtime_dir/provider-mixed.sql"
mixed=$(cli 'sccp reload')
assert_contains mixed "$mixed" 'Reload failed:'
after_mixed_devices=$(cli 'sccp show devices')
after_mixed_lines=$(cli 'sccp show lines')
assert_contains mixed-live-device "$after_mixed_devices" 'SEP000000000003'
assert_contains mixed-live-line "$after_mixed_lines" 'Complete å'

cli 'module unload chan_sccp2.so' >/dev/null
printf 'RT-001 PASS backend=%s\n' "$backend"
