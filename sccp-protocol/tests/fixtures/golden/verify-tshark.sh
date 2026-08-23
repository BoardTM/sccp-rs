#!/usr/bin/env bash
set -euo pipefail

for required_command in xxd text2pcap tshark; do
    command -v "$required_command" >/dev/null || {
        echo "required command not found: $required_command" >&2
        exit 1
    }
done

fixture_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
temporary_dir=$(mktemp -d "${TMPDIR:-/tmp}/sccp-golden-tshark.XXXXXX")
trap 'rm -rf "$temporary_dir"' EXIT

verify_fixture() {
    local name=$1
    shift
    local raw="$temporary_dir/$name.bin"
    local dump="$temporary_dir/$name.txt"
    local capture="$temporary_dir/$name.pcapng"
    local details

    xxd -r -p "$fixture_dir/$name.hex" "$raw"
    xxd -g1 "$raw" "$dump"
    text2pcap -q -T 2000,2000 "$dump" "$capture"
    details=$(tshark -r "$capture" -d tcp.port==2000,skinny -V)

    for expected in "$@"; do
        if ! grep -Fq "$expected" <<<"$details"; then
            echo "$name: Wireshark did not report: $expected" >&2
            exit 1
        fi
    done
    echo "$name: verified"
}

verify_fixture start_media_transmission_v17 \
    "Message ID: StartMediaTransmission (138)" \
    "conferenceId: 27110996" \
    "remoteIpAddr IPv4 Address: 192.168.9.44" \
    "remotePortNumber: 19654" \
    "callReference: 27110996"

verify_fixture open_receive_channel_v17 \
    "Message ID: OpenReceiveChannel (261)" \
    "conferenceId: 23641324" \
    "sourceIpAddr IPv4 Address: 192.168.9.44" \
    "sourcePortNumber: 4000" \
    "callReference: 23641324"

verify_fixture start_media_transmission_ack_v20 \
    "Message ID: StartMediaTransmissionAck (340)" \
    "callReference: 18" \
    "transmitIpAddr IPv4 Address: 10.1.2.44" \
    "portNumber: 12276" \
    "mediaTransmissionStatus: Ok (0x00000000)"
