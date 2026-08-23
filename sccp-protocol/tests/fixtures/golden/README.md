# SCCP golden bytes

These copied fixtures are independent, fixed compatibility inputs for the Rust
codec. They are deliberately not generated from `sccp-protocol` output.

`manifest.toml` records the direction, message ID, protocol version, and SHA-256
for every fixture.

## Verification

The Rust integration tests decode the exact frames, assert their semantic
values, and round-trip their private wire schemas byte-for-byte:

```sh
cargo test -p sccp-protocol --test reference_fixtures
```

When Wireshark's command-line tools are installed, its Skinny dissector can
independently check the message IDs, addresses, ports, and call identifiers:

```sh
./sccp-protocol/tests/fixtures/golden/verify-tshark.sh
```

## Adding live captures

When a real phone is available, capture port 2000 traffic to a pcapng file and
keep the original capture outside Git until it has been reviewed for phone
numbers, names, IP addresses, credentials, and XML application data:

```sh
tshark -i en0 -f 'tcp port 2000' -w skinny-session.pcapng
tshark -r skinny-session.pcapng -Y skinny -V
```

Add only the smallest sanitized full SCCP frames needed to prove a layout.
Record the phone model, firmware, direction, negotiated version, and checksum.
