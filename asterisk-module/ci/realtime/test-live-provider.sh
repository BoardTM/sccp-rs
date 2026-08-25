#!/bin/sh
set -eu

backend=${1:-}
module_path=${2:-}
realtime_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
. "$realtime_dir/../test-support/asterisk-sandbox.sh"
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

test_root=
asterisk_log=
cli_log=

finish() {
	status=$1
	trap - EXIT HUP INT TERM
	sccp_sandbox_stop
	if [ "$status" -ne 0 ]; then
		sccp_sandbox_diagnostics "$cli_log" "$asterisk_log"
	fi
	sccp_sandbox_cleanup
	exit "$status"
}
trap 'finish $?' EXIT
trap 'exit 130' HUP INT TERM

sccp_sandbox_create "chan-sccp2-realtime-${backend}" "$module_path" \
	"$asterisk_module_dir" "$asterisk_data_dir"
test_root=$SCCP_SANDBOX_ROOT
asterisk_log="$test_root/asterisk.log"
cli_log="$test_root/cli.log"

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

sccp_sandbox_start "$asterisk_bin" "$test_root/etc/sccp.conf" "$asterisk_log"

cli() {
	command=$1
	printf '\n%s\n' "$command" >>"$cli_log"
	if output=$(sccp_sandbox_cli "$asterisk_bin" "$command" 2>&1); then
		status=0
	else
		status=$?
	fi
	printf '%s\n' "$output" >>"$cli_log"
	printf '%s\n' "$output"
	return "$status"
}

if ! sccp_sandbox_wait_ready "$asterisk_bin"; then
	if [ "$SCCP_SANDBOX_READY_FAILURE" = exited ]; then
		printf 'Asterisk exited during startup (status %s)\n' \
			"$SCCP_SANDBOX_EXIT_STATUS" >&2
	else
		printf 'Asterisk did not become ready within 10 seconds\n' >&2
	fi
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

module_load=$(cli 'module load chan_sccp2.so')
assert_contains module-load "$module_load" 'Loaded chan_sccp2.so'
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

module_unload=$(cli 'module unload chan_sccp2.so')
assert_contains module-unload "$module_unload" 'Unloaded chan_sccp2.so'
printf 'RT-001 PASS backend=%s\n' "$backend"
