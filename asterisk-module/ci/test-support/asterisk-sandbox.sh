#!/bin/sh
# Shared isolated-Asterisk lifecycle for native integration harnesses.

sccp_sandbox_create() {
	prefix=$1
	module_path=$2
	module_dir=$3
	data_dir=$4
	SCCP_SANDBOX_ROOT=
	SCCP_SANDBOX_ROOT=$(mktemp -d "/tmp/${prefix}.XXXXXX") || return
	SCCP_SANDBOX_PID=
	SCCP_SANDBOX_ASTERISK_CONF="$SCCP_SANDBOX_ROOT/etc/asterisk.conf"
	if ! mkdir -p \
		"$SCCP_SANDBOX_ROOT/etc" \
		"$SCCP_SANDBOX_ROOT/modules" \
		"$SCCP_SANDBOX_ROOT/var/db" \
		"$SCCP_SANDBOX_ROOT/var/key" \
		"$SCCP_SANDBOX_ROOT/var/lib" \
		"$SCCP_SANDBOX_ROOT/var/log" \
		"$SCCP_SANDBOX_ROOT/var/run" \
		"$SCCP_SANDBOX_ROOT/var/spool"; then
		sccp_sandbox_cleanup
		return 1
	fi
	if ! sccp_sandbox_populate "$module_path" "$module_dir" "$data_dir"; then
		sccp_sandbox_cleanup
		return 1
	fi
}

sccp_sandbox_populate() {
	module_path=$1
	module_dir=$2
	data_dir=$3
	for installed_module in "$module_dir"/*.so; do
		[ -f "$installed_module" ] || continue
		installed_name=$(basename "$installed_module")
		[ "$installed_name" = chan_sccp2.so ] && continue
		ln -s "$installed_module" "$SCCP_SANDBOX_ROOT/modules/$installed_name" \
			|| return 1
	done
	if ! cp "$module_path" "$SCCP_SANDBOX_ROOT/modules/chan_sccp2.so" \
		|| ! sccp_sandbox_write_config "$data_dir"; then
		return 1
	fi
}

sccp_sandbox_write_config() {
	data_dir=$1
	{
		printf '%s\n' '[directories]'
		printf 'astetcdir => %s/etc\n' "$SCCP_SANDBOX_ROOT"
		printf 'astmoddir => %s/modules\n' "$SCCP_SANDBOX_ROOT"
		printf 'astvarlibdir => %s/var/lib\n' "$SCCP_SANDBOX_ROOT"
		printf 'astdbdir => %s/var/db\n' "$SCCP_SANDBOX_ROOT"
		printf 'astkeydir => %s/var/key\n' "$SCCP_SANDBOX_ROOT"
		printf 'astdatadir => %s\n' "$data_dir"
		printf 'astagidir => %s/var/lib/agi-bin\n' "$SCCP_SANDBOX_ROOT"
		printf 'astspooldir => %s/var/spool\n' "$SCCP_SANDBOX_ROOT"
		printf 'astrundir => %s/var/run\n' "$SCCP_SANDBOX_ROOT"
		printf 'astlogdir => %s/var/log\n' "$SCCP_SANDBOX_ROOT"
		printf '%s\n' '' '[options]' 'verbose = 0' 'debug = 0' \
			'nocolor = yes' 'documentation_language = en_US'
	} >"$SCCP_SANDBOX_ASTERISK_CONF"
}

sccp_sandbox_start() {
	asterisk_bin=$1
	config_path=$2
	log_path=$3
	SCCP_CONFIG=$config_path \
		"$asterisk_bin" -C "$SCCP_SANDBOX_ASTERISK_CONF" -f -g -q \
		>"$log_path" 2>&1 &
	SCCP_SANDBOX_PID=$!
}

sccp_sandbox_cli() {
	asterisk_bin=$1
	shift
	"$asterisk_bin" -C "$SCCP_SANDBOX_ASTERISK_CONF" -rx "$*"
}

sccp_sandbox_wait_ready() {
	asterisk_bin=$1
	SCCP_SANDBOX_READY_FAILURE=
	SCCP_SANDBOX_EXIT_STATUS=
	SCCP_SANDBOX_READY_PID=
	SCCP_SANDBOX_READY_OUTPUT="$SCCP_SANDBOX_ROOT/fully-booted.log"
	attempt=0
	while [ "$attempt" -lt 100 ]; do
		if ! kill -0 "$SCCP_SANDBOX_PID" 2>/dev/null; then
			if [ -n "$SCCP_SANDBOX_READY_PID" ]; then
				kill "$SCCP_SANDBOX_READY_PID" 2>/dev/null || true
				wait "$SCCP_SANDBOX_READY_PID" 2>/dev/null || true
			fi
			if wait "$SCCP_SANDBOX_PID"; then
				SCCP_SANDBOX_EXIT_STATUS=0
			else
				SCCP_SANDBOX_EXIT_STATUS=$?
			fi
			SCCP_SANDBOX_PID=
			SCCP_SANDBOX_READY_FAILURE=exited
			return 1
		fi
		if [ -z "$SCCP_SANDBOX_READY_PID" ]; then
			if sccp_sandbox_cli "$asterisk_bin" 'core show uptime' >/dev/null 2>&1; then
				# The control socket opens before the module loader is ready. Loading a
				# DSO in that window makes Asterisk treat its constructor as built-in.
				"$asterisk_bin" -C "$SCCP_SANDBOX_ASTERISK_CONF" \
					-rx 'core waitfullybooted' >"$SCCP_SANDBOX_READY_OUTPUT" 2>&1 &
				SCCP_SANDBOX_READY_PID=$!
			fi
		elif ! kill -0 "$SCCP_SANDBOX_READY_PID" 2>/dev/null; then
			if wait "$SCCP_SANDBOX_READY_PID" \
				&& grep -Fq 'Asterisk has fully booted.' "$SCCP_SANDBOX_READY_OUTPUT"; then
				return 0
			fi
			SCCP_SANDBOX_READY_PID=
		fi
		attempt=$((attempt + 1))
		sleep 0.1
	done
	if [ -n "$SCCP_SANDBOX_READY_PID" ]; then
		kill "$SCCP_SANDBOX_READY_PID" 2>/dev/null || true
		wait "$SCCP_SANDBOX_READY_PID" 2>/dev/null || true
	fi
	SCCP_SANDBOX_READY_FAILURE=timeout
	return 1
}

sccp_sandbox_stop() {
	if [ -n "${SCCP_SANDBOX_PID:-}" ] \
		&& kill -0 "$SCCP_SANDBOX_PID" 2>/dev/null; then
		kill "$SCCP_SANDBOX_PID" 2>/dev/null || true
		wait "$SCCP_SANDBOX_PID" 2>/dev/null || true
	fi
	SCCP_SANDBOX_PID=
}

sccp_sandbox_diagnostics() {
	for diagnostic in "$@"; do
		[ ! -f "$diagnostic" ] || { printf '\n==> %s <==\n' "$diagnostic" >&2; cat "$diagnostic" >&2; }
	done
}

sccp_sandbox_cleanup() {
	sccp_sandbox_stop
	if [ -n "${SCCP_SANDBOX_ROOT:-}" ] && [ -d "$SCCP_SANDBOX_ROOT" ]; then
		rm -rf "$SCCP_SANDBOX_ROOT"
	fi
	SCCP_SANDBOX_ROOT=
}
