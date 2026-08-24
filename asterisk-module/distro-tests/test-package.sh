#!/bin/sh
set -eu

package_family=${1:-}
expected_major=${2:-}
module_path=${3:-}

if [ -z "$expected_major" ] || [ -z "$module_path" ] || [ ! -f "$module_path" ]; then
	printf 'usage: %s <deb|rpm> <asterisk-major> /path/to/chan_sccp2.so\n' "$0" >&2
	exit 2
fi

case "$package_family" in
deb)
	printf '#!/bin/sh\nexit 101\n' >/usr/sbin/policy-rc.d
	chmod +x /usr/sbin/policy-rc.d
	export DEBIAN_FRONTEND=noninteractive
	apt-get update
	apt-get install -y --no-install-recommends asterisk findutils
	;;
rpm)
	dnf --assumeyes --setopt=install_weak_deps=False install asterisk findutils
	;;
*)
	printf 'unsupported package family: %s\n' "$package_family" >&2
	exit 2
	;;
esac

asterisk_bin=/usr/sbin/asterisk
installed_version=$($asterisk_bin -V)
case "$installed_version" in
"Asterisk $expected_major".*) ;;
*)
	printf 'expected packaged Asterisk %s, found: %s\n' \
		"$expected_major" "$installed_version" >&2
	exit 1
	;;
esac

asterisk_module_dir=
for candidate in \
	/usr/lib64/asterisk/modules \
	/usr/lib/x86_64-linux-gnu/asterisk/modules \
	/usr/lib/aarch64-linux-gnu/asterisk/modules \
	/usr/lib/asterisk/modules; do
	if [ -d "$candidate" ]; then
		asterisk_module_dir=$candidate
		break
	fi
done
if [ -z "$asterisk_module_dir" ]; then
	printf 'packaged Asterisk module directory was not found\n' >&2
	exit 1
fi

if ldd "$module_path" 2>&1 | tee /tmp/chan-sccp2-ldd | grep -q 'not found'; then
	printf 'release artifact has unresolved runtime libraries\n' >&2
	exit 1
fi

printf 'Testing %s with %s\n' "$(awk -F= '$1 == "PRETTY_NAME" { gsub(/^"|"$/, "", $2); print $2 }' /etc/os-release)" "$installed_version"
ASTERISK_BIN=$asterisk_bin \
ASTERISK_MODULE_DIR=$asterisk_module_dir \
ASTERISK_DATA_DIR=/usr/share/asterisk \
SCCP_LIFECYCLE_WARMUP_CYCLES=1 \
SCCP_LIFECYCLE_BATCH_CYCLES=1 \
	/workspace/asterisk-module/test-native-lifecycle.sh "$module_path"
