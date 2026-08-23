#!/bin/sh
set -eu

realtime_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
sqlite_database=$(mktemp "${TMPDIR:-/tmp}/sccp2-realtime.XXXXXX")
postgres_container="sccp2-realtime-postgres-$$"
mysql_container="sccp2-realtime-mysql-$$"

cleanup() {
    rm -f "$sqlite_database"
    docker rm -f "$postgres_container" "$mysql_container" >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

expected=$(printf '%s\n' \
    'initial|device|1|1|SEP001|button|line,1000' \
    'initial|device|2|1|SEP001|button|speed_dial,Support,2000' \
    'initial|device|3|1|SEP001|description|<NULL>' \
    'initial|device|4|1|SEP001|label|' \
    'initial|line|1|1|1000|label|Reception' \
    'initial|line|1000001|1|1001|_delete|yes' \
    'staged|1|1|1' \
    'rollback|1|1|0' \
    'refresh|3|3' \
    'refresh|device|1|3|SEP003|button|6c696e652c33303030' \
    'refresh|line|1|3|3000|label|436f6d706c657465')

assert_output() {
    backend=$1
    actual=$2
    if [ "$actual" != "$expected" ]; then
        printf '%s integration output differed\nexpected:\n%s\nactual:\n%s\n' \
            "$backend" "$expected" "$actual" >&2
        return 1
    fi
}

sqlite3 -batch "$sqlite_database" < "$realtime_dir/sqlite/001_initial.up.sql"
sqlite_output=$(sqlite3 -batch -separator '|' "$sqlite_database" < "$realtime_dir/integration.sql")
assert_output sqlite "$sqlite_output"
sqlite3 -batch "$sqlite_database" < "$realtime_dir/sqlite/001_initial.down.sql"

docker run --detach --rm --name "$postgres_container" \
    --env POSTGRES_PASSWORD=realtime \
    --env POSTGRES_DB=realtime \
    postgres:17-alpine >/dev/null
until docker exec "$postgres_container" pg_isready --username postgres --dbname realtime >/dev/null 2>&1; do
    sleep 1
done
docker exec --interactive "$postgres_container" psql \
    --username postgres --dbname realtime --quiet --set ON_ERROR_STOP=1 \
    < "$realtime_dir/postgresql/001_initial.up.sql" >/dev/null
postgres_output=$(docker exec --interactive "$postgres_container" psql \
    --username postgres --dbname realtime --tuples-only --no-align \
    --field-separator '|' --quiet --set ON_ERROR_STOP=1 \
    < "$realtime_dir/integration.sql")
assert_output postgresql "$postgres_output"
docker exec --interactive "$postgres_container" psql \
    --username postgres --dbname realtime --quiet --set ON_ERROR_STOP=1 \
    < "$realtime_dir/postgresql/001_initial.down.sql" >/dev/null

docker run --detach --rm --name "$mysql_container" \
    --env MYSQL_ROOT_PASSWORD=realtime \
    --env MYSQL_DATABASE=realtime \
    mysql:8.4 >/dev/null
until docker exec --env MYSQL_PWD=realtime "$mysql_container" mysql \
    --user=root --database=realtime --execute 'SELECT 1' >/dev/null 2>&1; do
    sleep 1
done
docker exec --interactive --env MYSQL_PWD=realtime "$mysql_container" mysql \
    --user=root --database=realtime \
    < "$realtime_dir/mysql/001_initial.up.sql"
mysql_output=$(docker exec --interactive --env MYSQL_PWD=realtime "$mysql_container" mysql \
    --user=root --database=realtime --batch --raw --skip-column-names \
    < "$realtime_dir/integration.sql" | tr '\t' '|')
assert_output mysql "$mysql_output"
docker exec --interactive --env MYSQL_PWD=realtime "$mysql_container" mysql \
    --user=root --database=realtime \
    < "$realtime_dir/mysql/001_initial.down.sql"
