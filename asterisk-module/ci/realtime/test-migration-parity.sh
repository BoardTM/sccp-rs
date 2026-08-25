#!/bin/sh
set -eu

realtime_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
fixture_root=$(mktemp -d /tmp/sccp-migration-parity.XXXXXX)
cleanup() {
	rm -rf "$fixture_root"
}
trap cleanup EXIT HUP INT TERM

for dialect in sqlite postgresql mysql; do
	mkdir -p "$fixture_root/$dialect"
	cp "$realtime_dir/$dialect/001_initial.up.sql" "$fixture_root/$dialect/"
	cp "$realtime_dir/$dialect/001_initial.down.sql" "$fixture_root/$dialect/"
done

check_fixture() {
	SCCP_REALTIME_DIR=$fixture_root \
		"$realtime_dir/check-migration-parity.sh" "$realtime_dir/schema.manifest"
}

check_fixture >/dev/null

sed 's/    created_at TIMESTAMP/    dialect_only TEXT,\
    created_at TIMESTAMP/' \
	"$fixture_root/mysql/001_initial.up.sql" >"$fixture_root/mysql/mutated.sql"
mv "$fixture_root/mysql/mutated.sql" "$fixture_root/mysql/001_initial.up.sql"
if check_fixture >"$fixture_root/column.out" 2>&1; then
	printf 'parity checker accepted a dialect-only column\n' >&2
	exit 1
fi
grep -q 'columns differ' "$fixture_root/column.out"

cp "$realtime_dir/mysql/001_initial.up.sql" "$fixture_root/mysql/001_initial.up.sql"
sed 's/    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP/    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP CHECK (created_at IS NOT NULL)/' \
	"$fixture_root/postgresql/001_initial.up.sql" >"$fixture_root/postgresql/mutated.sql"
mv "$fixture_root/postgresql/mutated.sql" "$fixture_root/postgresql/001_initial.up.sql"
if check_fixture >"$fixture_root/constraint.out" 2>&1; then
	printf 'parity checker accepted a dialect-only constraint\n' >&2
	exit 1
fi
grep -q 'constraints differ' "$fixture_root/constraint.out"

cp "$realtime_dir/postgresql/001_initial.up.sql" \
	"$fixture_root/postgresql/001_initial.up.sql"
sed 's/    field_value TEXT,/    field_value TEXT NOT NULL,/' \
	"$fixture_root/sqlite/001_initial.up.sql" >"$fixture_root/sqlite/mutated.sql"
mv "$fixture_root/sqlite/mutated.sql" "$fixture_root/sqlite/001_initial.up.sql"
if check_fixture >"$fixture_root/nullability.out" 2>&1; then
	printf 'parity checker accepted dialect-only nullability\n' >&2
	exit 1
fi
grep -q 'column contracts differ' "$fixture_root/nullability.out"

cp "$realtime_dir/sqlite/001_initial.up.sql" "$fixture_root/sqlite/001_initial.up.sql"
sed 's/FROM sccp2_realtime_active_generation/    NULL AS dialect_only,\
FROM sccp2_realtime_active_generation/' \
	"$fixture_root/mysql/001_initial.up.sql" >"$fixture_root/mysql/mutated.sql"
mv "$fixture_root/mysql/mutated.sql" "$fixture_root/mysql/001_initial.up.sql"
if check_fixture >"$fixture_root/view.out" 2>&1; then
	printf 'parity checker accepted a dialect-only view projection\n' >&2
	exit 1
fi
grep -q 'projection differs' "$fixture_root/view.out"

printf 'realtime migration parity regression tests passed\n'
