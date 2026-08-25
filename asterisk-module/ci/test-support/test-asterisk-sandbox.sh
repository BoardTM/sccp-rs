#!/bin/sh
set -eu

support_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
. "$support_dir/asterisk-sandbox.sh"

fixture_root=$(mktemp -d "${TMPDIR:-/tmp}/sccp-sandbox-ready.XXXXXX")
server_pid=
boot_release_pid=

cleanup() {
	status=$?
	trap - EXIT HUP INT TERM
	for pid in "$boot_release_pid" "$server_pid"; do
		if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
			kill "$pid" 2>/dev/null || true
			wait "$pid" 2>/dev/null || true
		fi
	done
	rm -rf "$fixture_root"
	exit "$status"
}
trap cleanup EXIT
trap 'exit 130' HUP INT TERM

fake_asterisk="$fixture_root/asterisk"
cat >"$fake_asterisk" <<'EOF'
#!/bin/sh
set -eu

command=
while [ "$#" -gt 0 ]; do
	case "$1" in
	-rx)
		shift
		command=${1:-}
		break
		;;
	*) shift ;;
	esac
done
printf '%s\n' "$command" >>"$SCCP_SANDBOX_TEST_COMMANDS"

case "$command" in
'core show uptime')
	printf 'System uptime: immediately reachable\n'
	;;
'core waitfullybooted')
	: >"$SCCP_SANDBOX_TEST_WAIT_STARTED"
	while [ ! -f "$SCCP_SANDBOX_TEST_BOOTED" ]; do
		sleep 0.05
	done
	printf 'Asterisk has fully booted.\n'
	;;
*)
	printf 'unexpected fake Asterisk command: %s\n' "$command" >&2
	exit 2
	;;
esac
EOF
chmod +x "$fake_asterisk"

run_server() {
	sleep 30 &
	server_pid=$!
	SCCP_SANDBOX_PID=$server_pid
}

delayed_root="$fixture_root/delayed"
mkdir -p "$delayed_root"
SCCP_SANDBOX_ROOT=$delayed_root
SCCP_SANDBOX_ASTERISK_CONF="$delayed_root/asterisk.conf"
SCCP_SANDBOX_TEST_COMMANDS="$delayed_root/commands.log"
SCCP_SANDBOX_TEST_WAIT_STARTED="$delayed_root/wait-started"
SCCP_SANDBOX_TEST_BOOTED="$delayed_root/fully-booted"
export SCCP_SANDBOX_TEST_COMMANDS SCCP_SANDBOX_TEST_WAIT_STARTED \
	SCCP_SANDBOX_TEST_BOOTED

run_server
(
	sleep 0.4
	: >"$SCCP_SANDBOX_TEST_BOOTED"
) &
boot_release_pid=$!

sccp_sandbox_wait_ready "$fake_asterisk"
[ -f "$SCCP_SANDBOX_TEST_WAIT_STARTED" ]
[ -f "$SCCP_SANDBOX_TEST_BOOTED" ]
[ "$(sed -n '1p' "$SCCP_SANDBOX_TEST_COMMANDS")" = 'core show uptime' ]
[ "$(sed -n '2p' "$SCCP_SANDBOX_TEST_COMMANDS")" = 'core waitfullybooted' ]
sccp_sandbox_stop
server_pid=
wait "$boot_release_pid"
boot_release_pid=

exit_root="$fixture_root/server-exit"
mkdir -p "$exit_root"
SCCP_SANDBOX_ROOT=$exit_root
SCCP_SANDBOX_ASTERISK_CONF="$exit_root/asterisk.conf"
SCCP_SANDBOX_TEST_COMMANDS="$exit_root/commands.log"
SCCP_SANDBOX_TEST_WAIT_STARTED="$exit_root/wait-started"
SCCP_SANDBOX_TEST_BOOTED="$exit_root/never-fully-booted"
export SCCP_SANDBOX_TEST_COMMANDS SCCP_SANDBOX_TEST_WAIT_STARTED \
	SCCP_SANDBOX_TEST_BOOTED

(
	sleep 0.3
	exit 23
) &
server_pid=$!
SCCP_SANDBOX_PID=$server_pid

if sccp_sandbox_wait_ready "$fake_asterisk"; then
	printf 'readiness unexpectedly succeeded after the Asterisk process exited\n' >&2
	exit 1
fi
[ "$SCCP_SANDBOX_READY_FAILURE" = exited ]
[ "$SCCP_SANDBOX_EXIT_STATUS" -eq 23 ]
[ -z "$SCCP_SANDBOX_PID" ]
[ -f "$SCCP_SANDBOX_TEST_WAIT_STARTED" ]
server_pid=

printf 'Asterisk sandbox readiness contract passed\n'
