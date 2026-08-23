#!/bin/sh
set -eu

usage() {
	printf 'Usage: %s {22|23|22.x.y|23.x.y} [output-directory]\n' "$0" >&2
}

case "${1:-}" in
	22)
		asterisk_version=22.7.0
		asterisk_feature=asterisk-22
		;;
	23)
		asterisk_version=23.4.1
		asterisk_feature=asterisk-23
		;;
	22.* | 23.*)
		if ! printf '%s\n' "$1" | grep -Eq '^2[23]\.[0-9]+\.[0-9]+$'; then
			usage
			exit 2
		fi
		asterisk_version=$1
		asterisk_feature=asterisk-${1%%.*}
		;;
	-h | --help)
		usage
		exit 0
		;;
	*)
		usage
		exit 2
		;;
esac

asterisk_abi=${asterisk_version%%.*}

if ! command -v docker >/dev/null 2>&1; then
	printf 'error: Docker is required; install and start Docker Desktop first.\n' >&2
	exit 1
fi
if ! docker info >/dev/null 2>&1; then
	printf 'error: the Docker daemon is not running; start Docker Desktop first.\n' >&2
	exit 1
fi
if ! docker buildx version >/dev/null 2>&1; then
	printf 'error: Docker Buildx is required; update Docker Desktop first.\n' >&2
	exit 1
fi

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_dir=$(dirname -- "$script_dir")
output_dir=${2:-"$repo_dir/dist"}
mkdir -p "$output_dir"

docker buildx build \
	--pull \
	--platform linux/amd64 \
	--progress plain \
	--build-arg "ASTERISK_VERSION=$asterisk_version" \
	--build-arg "ASTERISK_FEATURE=$asterisk_feature" \
	--output "type=local,dest=$output_dir" \
	--file "$script_dir/Dockerfile.linux-x86_64" \
	"$repo_dir"

artifact="$output_dir/chan_sccp2-asterisk-${asterisk_abi}-linux-x86_64.so"
printf 'Built %s\n' "$artifact"
