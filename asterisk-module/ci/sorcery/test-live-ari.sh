#!/bin/sh
set -eu

module_path=${1:-}
sorcery_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
. "$sorcery_dir/../test-support/asterisk-sandbox.sh"
asterisk_bin=${ASTERISK_BIN:-/opt/asterisk-live/sbin/asterisk}
asterisk_module_dir=${ASTERISK_MODULE_DIR:-/opt/asterisk-live/lib/asterisk/modules}
asterisk_data_dir=${ASTERISK_DATA_DIR:-/opt/asterisk-live/var/lib/asterisk}
ari_port=${SCCP_ARI_TEST_PORT:-28088}
ari_base="http://127.0.0.1:$ari_port/ari"
ari_user=sccp-sorcery-test
ari_password=sccp-sorcery-secret

if [ -z "$module_path" ] || [ ! -f "$module_path" ]; then
	printf 'usage: %s /path/to/libchan_sccp2.so\n' "$0" >&2
	exit 2
fi
case "$ari_port" in
'' | *[!0-9]*)
	printf 'SCCP_ARI_TEST_PORT must be an unsigned decimal integer\n' >&2
	exit 2
	;;
esac
if [ "$ari_port" -lt 1 ] || [ "$ari_port" -gt 65535 ]; then
	printf 'SCCP_ARI_TEST_PORT is outside the permitted range\n' >&2
	exit 2
fi
if ! command -v curl >/dev/null 2>&1; then
	printf 'curl is required for the live Sorcery/ARI test\n' >&2
	exit 2
fi

test_root=
asterisk_log=
cli_log=
ari_log=

finish() {
	status=$1
	trap - EXIT HUP INT TERM
	sccp_sandbox_stop
	if [ "$status" -ne 0 ]; then
		sccp_sandbox_diagnostics "$ari_log" "$cli_log" "$asterisk_log"
	fi
	sccp_sandbox_cleanup
	exit "$status"
}
trap 'finish $?' EXIT
trap 'exit 130' HUP INT TERM

sccp_sandbox_create chan-sccp2-sorcery-ari "$module_path" \
	"$asterisk_module_dir" "$asterisk_data_dir"
test_root=$SCCP_SANDBOX_ROOT
asterisk_log="$test_root/asterisk.log"
cli_log="$test_root/cli.log"
ari_log="$test_root/ari.log"

for required_module in \
	res_sorcery_astdb.so res_sorcery_config.so res_http_websocket.so res_websocket_client.so \
	res_stasis.so res_ari_model.so res_ari.so res_ari_asterisk.so; do
	if [ ! -f "$test_root/modules/$required_module" ]; then
		printf 'Sorcery/ARI module is unavailable: %s\n' "$required_module" >&2
		exit 2
	fi
done

cat >"$test_root/etc/modules.conf" <<'EOF'
[modules]
autoload = no
load = res_sorcery_astdb.so
load = res_sorcery_config.so
load = res_http_websocket.so
load = res_websocket_client.so
load = res_stasis.so
load = res_ari_model.so
load = res_ari.so
load = res_ari_asterisk.so
noload = chan_sccp2.so
EOF

cat >"$test_root/etc/http.conf" <<EOF
[general]
enabled = yes
bindaddr = 127.0.0.1
bindport = $ari_port
EOF

cat >"$test_root/etc/ari.conf" <<EOF
[general]
enabled = yes
pretty = no

[$ari_user]
type = user
read_only = no
password = $ari_password
EOF

cat >"$test_root/etc/sorcery.conf" <<'EOF'
[chan_sccp2]
device = astdb,chan_sccp2
line = astdb,chan_sccp2
EOF

cat >"$test_root/etc/sccp.conf" <<'EOF'
[general]
configuration_source = sorcery
bind = 127.0.0.1:24997
advertised_address = 127.0.0.1
disallow = all
allow = ulaw
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

wait_cli_contains() {
	command=$1
	expected=$2
	attempt=0
	while [ "$attempt" -lt 100 ]; do
		if WAIT_OUTPUT=$(sccp_sandbox_cli "$asterisk_bin" "$command" 2>&1); then
			case "$WAIT_OUTPUT" in
			*"$expected"*) return 0 ;;
			esac
		fi
		attempt=$((attempt + 1))
		sleep 0.1
	done
	printf '\n%s\n%s\n' "$command" "$WAIT_OUTPUT" >>"$cli_log"
	return 1
}

wait_cli_not_contains() {
	command=$1
	unexpected=$2
	attempt=0
	while [ "$attempt" -lt 100 ]; do
		if WAIT_OUTPUT=$(sccp_sandbox_cli "$asterisk_bin" "$command" 2>&1); then
			case "$WAIT_OUTPUT" in
			*"$unexpected"*) ;;
			*) return 0 ;;
			esac
		fi
		attempt=$((attempt + 1))
		sleep 0.1
	done
	printf '\n%s\n%s\n' "$command" "$WAIT_OUTPUT" >>"$cli_log"
	return 1
}

wait_log_contains() {
	expected=$1
	attempt=0
	while [ "$attempt" -lt 100 ]; do
		if grep -Fq "$expected" "$asterisk_log"; then
			return 0
		fi
		attempt=$((attempt + 1))
		sleep 0.1
	done
	return 1
}

ari_request() {
	method=$1
	path=$2
	expected_status=$3
	payload=${4:-}
	response_file="$test_root/ari-response.json"
	if [ -n "$payload" ]; then
		status=$(curl --silent --show-error --output "$response_file" \
			--write-out '%{http_code}' --user "$ari_user:$ari_password" \
			--request "$method" --header 'Content-Type: application/json' \
			--data "$payload" "$ari_base$path")
	else
		status=$(curl --silent --show-error --output "$response_file" \
			--write-out '%{http_code}' --user "$ari_user:$ari_password" \
			--request "$method" "$ari_base$path")
	fi
	ARI_RESPONSE=$(cat "$response_file")
	printf '\n%s %s -> %s\n%s\n' "$method" "$path" "$status" "$ARI_RESPONSE" >>"$ari_log"
	if [ "$status" != "$expected_status" ]; then
		printf 'ARI %s %s returned %s, expected %s\n%s\n' \
			"$method" "$path" "$status" "$expected_status" "$ARI_RESPONSE" >&2
		exit 1
	fi
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

module_load=$(cli 'module load chan_sccp2.so')
assert_contains module-load "$module_load" 'Loaded chan_sccp2.so'

ari_request PUT '/asterisk/config/dynamic/chan_sccp2/line/1001' 200 \
	'{"fields":[{"attribute":"label","value":"Reception"},{"attribute":"context","value":"from-sccp"},{"attribute":"callerid","value":"Reception <1001>"}]}'
wait_cli_contains 'sccp show lines' '1001' || {
	printf 'Sorcery line did not converge\n' >&2
	exit 1
}
assert_contains line-label "$WAIT_OUTPUT" 'Reception'

ari_request PUT '/asterisk/config/dynamic/chan_sccp2/device/SEP001122334455' 200 \
	'{"fields":[{"attribute":"description","value":"Reception phone"},{"attribute":"button.0001","value":"line, 1001, label=Reception"},{"attribute":"button.0002","value":"speed_dial, Helpdesk, 2000"}]}'
wait_cli_contains 'sccp show devices' 'SEP001122334455' || {
	printf 'Sorcery device did not converge\n' >&2
	exit 1
}
device=$(cli 'sccp show devices SEP001122334455')
buttons=$(cli 'sccp show devices SEP001122334455 buttons')
assert_contains device-description "$device" 'Description: Reception phone'
assert_contains line-button "$buttons" 'Reception'
assert_contains speed-dial "$buttons" 'Helpdesk'

ari_request GET '/asterisk/config/dynamic/chan_sccp2/device/SEP001122334455' 200
assert_contains ari-get-device "$ARI_RESPONSE" 'button.0001'
assert_contains ari-get-device "$ARI_RESPONSE" 'button.0002'

ari_request PUT '/asterisk/config/dynamic/chan_sccp2/device/SEP001122334455' 200 \
	'{"fields":[{"attribute":"button.0002","value":""}]}'
wait_cli_not_contains 'sccp show devices SEP001122334455 buttons' 'Helpdesk' || {
	printf 'Indexed Sorcery tombstone did not converge\n' >&2
	exit 1
}
assert_contains tombstone-line-preserved "$WAIT_OUTPUT" 'Reception'
ari_request GET '/asterisk/config/dynamic/chan_sccp2/device/SEP001122334455' 200
assert_not_contains ari-tombstone "$ARI_RESPONSE" 'button.0002'

lkg_before=$(cli 'database get SCCP/config last-known-good')
assert_contains lkg-before "$lkg_before" 'SEP001122334455'

ari_request PUT '/asterisk/config/dynamic/chan_sccp2/device/SEP001122334455' 200 \
	'{"fields":[{"attribute":"button.0001","value":"line, 9999, label=Invalid"}]}'
wait_log_contains 'SCCP Sorcery reconciliation failed after' || {
	printf 'Invalid Sorcery candidate did not report reconciliation failure\n' >&2
	exit 1
}
ari_request GET '/asterisk/config/dynamic/chan_sccp2/device/SEP001122334455' 200
assert_contains invalid-desired-retained "$ARI_RESPONSE" '9999'
live_device=$(cli 'sccp show devices SEP001122334455')
live_buttons=$(cli 'sccp show devices SEP001122334455 buttons')
assert_contains invalid-live-device "$live_device" 'Description: Reception phone'
assert_contains invalid-live-line "$live_buttons" 'Reception'
assert_not_contains invalid-not-activated "$live_buttons" 'Invalid'
lkg_after=$(cli 'database get SCCP/config last-known-good')
if [ "$lkg_after" != "$lkg_before" ]; then
	printf 'last-known-good changed after invalid desired inventory\nbefore:\n%s\nafter:\n%s\n' \
		"$lkg_before" "$lkg_after" >&2
	exit 1
fi

ari_request DELETE '/asterisk/config/dynamic/chan_sccp2/device/SEP001122334455' 204
wait_cli_not_contains 'sccp show devices' 'SEP001122334455' || {
	printf 'Deleted Sorcery device remained live\n' >&2
	exit 1
}
ari_request DELETE '/asterisk/config/dynamic/chan_sccp2/line/1001' 204
wait_cli_not_contains 'sccp show lines' '1001' || {
	printf 'Deleted Sorcery line remained live\n' >&2
	exit 1
}

ari_request GET '/asterisk/config/dynamic/chan_sccp2/device/SEP001122334455' 404
ari_request GET '/asterisk/config/dynamic/chan_sccp2/line/1001' 404

module_unload=$(cli 'module unload chan_sccp2.so')
assert_contains module-unload "$module_unload" 'Unloaded chan_sccp2.so'
printf 'SORCERY-ARI-001 PASS\n'
