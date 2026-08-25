#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
realtime_dir=${SCCP_REALTIME_DIR:-$script_dir}
manifest=${1:-"$realtime_dir/schema.manifest"}

fail() {
	printf 'realtime migration parity: %s\n' "$*" >&2
	exit 1
}

sql_object() {
	kind=$1
	name=$2
	file=$3
	awk -v kind="$kind" -v name="$name" '
		BEGIN { capture = 0 }
		{
			line = tolower($0)
			if (line ~ "^create[[:space:]]+" kind "[[:space:]]+" name "([[:space:]]|$)") capture = 1
		}
		capture { print line }
		capture && line ~ /^\)[[:space:]]*(engine[[:space:]]*=.*)?;[[:space:]]*$/ { exit }
		capture && kind == "view" && line ~ /;[[:space:]]*$/ { exit }
	' "$file"
}

object_names() {
	kind=$1
	file=$2
	awk -v kind="$kind" '
		tolower($1) == "create" && tolower($2) == kind {
			name = tolower($3)
			sub(/[^a-z0-9_].*$/, "", name)
			print name
		}
	' "$file" | sort
}

manifest_names() {
	kind=$1
	awk -F '|' -v kind="$kind" '$1 == kind { print $2 }' "$manifest" | sort
}

table_columns() {
	awk '
		/^    [a-z_][a-z0-9_]*[[:space:]]/ {
			name = $1
			if (name != "primary" && name != "unique" && name != "foreign" && name != "check" && name != "constraint") print name
		}
	'
}

table_column_contracts() {
	awk '
		function flush() {
			if (name == "") return
			required = definition ~ /(not[[:space:]]+null|primary[[:space:]]+key)/ ? "required" : "nullable"
			defaulted = definition ~ /default[[:space:]]/ ? "default" : "no-default"
			print name ":" required ":" defaulted
			name = ""
			definition = ""
		}
		/^    [a-z_][a-z0-9_]*[[:space:]]/ {
			candidate = $1
			if (candidate == "primary" || candidate == "unique" || candidate == "foreign" || candidate == "check" || candidate == "constraint") {
				flush()
				next
			}
			flush()
			name = candidate
			definition = $0
			next
		}
		name != "" { definition = definition " " $0 }
		END { flush() }
	'
}

view_columns() {
	awk '
		/^select[[:space:]]*$/ { select = 1; next }
		select && /^from[[:space:]]/ { exit }
		select && /^    / {
			line = $0
			sub(/,[[:space:]]*$/, "", line)
			if (line ~ /[[:space:]]as[[:space:]][a-z_][a-z0-9_]*[[:space:]]*$/) {
				n = split(line, part, /[[:space:]]+/)
				print part[n]
			} else {
				print "!unaliased-projection"
			}
		}
	'
}

join_lines() {
	awk 'BEGIN { first = 1 } { if (!first) printf " "; printf "%s", $0; first = 0 } END { print "" }'
}

normalize_words() {
	for word in $1; do
		printf '%s\n' "$word"
	done | sort | join_lines
}

regex_count() {
	pattern=$1
	awk -v pattern="$pattern" '
		{ line = line " " $0 }
		END {
			count = 0
			while (match(line, pattern)) {
				count++
				line = substr(line, RSTART + RLENGTH)
			}
			print count
		}
	'
}

has_pattern() {
	pattern=$1
	grep -Eq "$pattern"
}

constraint_intents() {
	object=$1
	name=$2
	intents=
	primary_count=$(printf '%s\n' "$object" | regex_count 'primary[[:space:]]+key')
	unique_count=$(printf '%s\n' "$object" | regex_count 'unique([^a-z0-9_]|$)')
	foreign_count=$(printf '%s\n' "$object" | regex_count 'references[[:space:]]+[a-z_]')
	cascade_count=$(printf '%s\n' "$object" | regex_count 'on[[:space:]]+delete[[:space:]]+cascade')
	intents="primary-key=$primary_count unique=$unique_count foreign-key=$foreign_count cascade=$cascade_count"

	check_count=$(printf '%s\n' "$object" | regex_count 'check[[:space:]]*[(]')
	intents="$intents checks=$check_count"
	for specification in \
		"id-positive-check|check[[:space:]]*\([[:space:]]*id[[:space:]]*>[[:space:]]*0" \
		"singleton-check|check[[:space:]]*\([[:space:]]*singleton[[:space:]]*=[[:space:]]*(1|true)" \
		"family-check|check[[:space:]]*\([[:space:]]*family[[:space:]]+in[[:space:]]*\([[:space:]]*'device',[[:space:]]*'line'" \
		"name-nonblank-check|check[[:space:]]*\([[:space:]]*(length|char_length)[[:space:]]*\([[:space:]]*(btrim|trim)[[:space:]]*\([[:space:]]*(name|field_name)" \
		"position-check|(section_position[[:space:]]*<=[[:space:]]*9223372036853|field_position[[:space:]]*<[[:space:]]*1000000)" \
		"reserved-field-check|field_name[[:space:]]+not[[:space:]]+in[[:space:]]*\(" \
		"delete-value-check|field_name[[:space:]]*<>[[:space:]]*'_delete'"
	do
		intent=${specification%%|*}
		pattern=${specification#*|}
		if printf '%s\n' "$object" | tr '\n' ' ' | has_pattern "$pattern"; then
			intents="$intents $intent"
		fi
	done
	normalize_words "$intents"
}

for dialect in sqlite postgresql mysql; do
	up="$realtime_dir/$dialect/001_initial.up.sql"
	for kind in table view; do
		expected=$(manifest_names "$kind" | join_lines)
		actual=$(object_names "$kind" "$up" | join_lines)
		[ "$actual" = "$expected" ] \
			|| fail "$dialect $kind set differs: expected [$expected], got [$actual]"
	done
done

while IFS='|' read -r kind name columns intents column_contracts; do
	case "$kind" in ''|'#'*) continue ;; esac
	for dialect in sqlite postgresql mysql; do
		up="$realtime_dir/$dialect/001_initial.up.sql"
		down="$realtime_dir/$dialect/001_initial.down.sql"
		object=$(sql_object "$kind" "$name" "$up")
		[ -n "$object" ] || fail "$dialect is missing $kind $name"
		case "$kind" in
		table)
			actual_columns=$(printf '%s\n' "$object" | table_columns | join_lines)
			[ "$actual_columns" = "$columns" ] \
				|| fail "$dialect $name columns differ: expected [$columns], got [$actual_columns]"
			actual_contracts=$(printf '%s\n' "$object" | table_column_contracts | join_lines)
			[ "$actual_contracts" = "$column_contracts" ] \
				|| fail "$dialect $name column contracts differ: expected [$column_contracts], got [$actual_contracts]"
			expected_intents=$(normalize_words "$intents")
			actual_intents=$(constraint_intents "$object" "$dialect $name")
			[ "$actual_intents" = "$expected_intents" ] \
				|| fail "$dialect $name constraints differ: expected [$expected_intents], got [$actual_intents]"
			;;
		view)
			actual_columns=$(printf '%s\n' "$object" | view_columns | join_lines)
			[ "$actual_columns" = "$columns" ] \
				|| fail "$dialect $name projection differs: expected [$columns], got [$actual_columns]"
			where_count=$(printf '%s\n' "$object" | regex_count 'where[[:space:]]')
			family_count=$(printf '%s\n' "$object" | regex_count "where[[:space:]]+section.family[[:space:]]*=[[:space:]]*'$intents'[[:space:]]*;")
			[ "$where_count" -eq 1 ] && [ "$family_count" -eq 1 ] \
				|| fail "$dialect $name must have exactly the $intents family predicate"
			;;
		esac
		drop_count=$(awk -v kind="$kind" -v name="$name" '
			{
				line = tolower($0)
				if (line ~ "drop[[:space:]]+" kind "([[:space:]]+if[[:space:]]+exists)?[[:space:]]+" name "([^a-z0-9_]|$)") count++
			}
			END { print count + 0 }
		' "$down")
		[ "$drop_count" -eq 1 ] \
			|| fail "$dialect down migration must drop $kind $name exactly once"
	done
done < "$manifest"

printf 'realtime migration schema parity passed\n'
