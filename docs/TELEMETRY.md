# Debug telemetry

`chan_sccp2` is published in two forms: the ordinary module and an opt-in
debug telemetry module. The debug module behaves like the ordinary channel
driver, but also keeps a small, bounded diagnostic history in memory and sends
a report to the project diagnostic service when `chan_sccp2` logs a warning or
error. The ordinary module does not contain the telemetry client or its
dependency graph.

> **Important:** The regular build contains NO telemetry whatsoever. Telemetry
> is compiled ONLY into the opt-in debug build for development and diagnostics.
> If you are running the regular build, NOTHING is sent back.

Installing the debug artifact is the opt-in. There is no background upload of
every log line: recent data is collected in memory and a report is created only
when a module warning or error triggers it.

## What a report contains

A triggered report can contain two linked events:

- A diagnostic snapshot with the triggering warning or error, up to 128 recent
  module log entries, the effective non-credential configuration, and bounded
  live device, call, channel, bridge, forwarding, parking, and media state.
- A signaling capture with recent decrypted SCCP frames in both directions,
  connection metadata, message IDs and names, device/session identity when
  known, observation-loss counters, and connection-end reasons such as peer
  closure, I/O failure, keepalive expiry, or deliberate server retirement.

The diagnostic data can include raw device IDs, line names, caller/called/dialed
numbers, firmware information, configuration paths, and network or media
endpoints. It also includes the module version and a stable SHA-256 hash of the
PBX installation UUID so reports from one installation can be correlated
without sending the UUID itself.

## Redaction and limits

The signaling capture is taken after transport decryption, so TLS does not hide
SCCP content from it. Before a frame enters the capture:

- known media key and salt reservoirs are zeroed;
- known credential-capable service payloads are suppressed;
- incomplete frames are fully suppressed; and
- complete unknown frames are retained exactly because their schema is not
  available for field-level redaction.

Credential contents, arbitrary channel-variable values, media keys and salts,
and RTP payloads are not included. RTP/audio/video traffic itself is not
captured.

All queues and snapshots are bounded. The signaling history retains at most
256 packet/disconnect records and 512 KiB, and each uploaded event is limited
to 1 MiB. Old records and overloaded queue entries are dropped rather than
blocking phone traffic. The module does not create a durable local capture or
spool.

## Delivery and use

Reports are sent over TLS WebSocket to `sccp.dbg.coral.works`. Delivery is
best-effort: transient failures are retried with bounded backoff, after which
the report is discarded.

Published debug assets are named
`chan_sccp2-asterisk-debug-linux-<architecture>-v<version>.so`. Install the
selected file as `chan_sccp2.so`, just like the ordinary artifact. Use it only
when you intend to share the diagnostic data described above; reinstall the
ordinary artifact to disable telemetry. See [INSTALL.md](INSTALL.md) for the
normal installation procedure.
