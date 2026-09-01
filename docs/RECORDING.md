# Call recording

`chan_sccp2` can record an SCCP call through Asterisk's MixMonitor service.
Recording can be controlled from a physical feature button, an opt-in `monitor`
soft key, or the `SCCPRecording` AMI action. These controls share the same
driver-owned recording session, so a call started from the handset can also be
stopped through AMI, and vice versa.

The driver owns at most one recording session per PBX call. It does not manage
MixMonitor sessions started independently by the dialplan or Asterisk's native
MixMonitor AMI actions.

## Requirements

The Asterisk `app_mixmonitor.so` module must be loaded. It is an optional module
dependency: `chan_sccp2` can load without it, but recording attempts will fail.
Check it from the Asterisk CLI:

```text
pbx*CLI> module show like app_mixmonitor.so
```

If it is installed but not running, load it and add it to the site's normal
module-loading configuration:

```text
pbx*CLI> module load app_mixmonitor.so
```

The Asterisk process must also be able to write to its configured monitoring
directory and the filesystem must have enough free space. The usual directory
is `/var/spool/asterisk/monitor`, but the effective path is controlled by the
Asterisk installation rather than `sccp.conf`.

Recording requires audio to pass through Asterisk. If a call is using direct
media, the driver first moves it back to an Asterisk media anchor, starts
MixMonitor only after that succeeds, and releases the recording anchor after
recording stops. The previous direct path is restored when no other feature
still requires anchored media. Failure to anchor media or publish the handset
recording state makes the start fail instead of leaving a partially established
recording.

## Configure handset controls

### Physical recording button

Add this canonical button entry to a device section:

```ini
[SEP001122334455]
type = device
button = line, 1001, label=Reception
button = feature, Record calls, monitor
```

The three comma-separated fields are fixed:

1. `feature` selects the feature-button namespace.
2. `Record calls` is the label shown by the phone.
3. `monitor` selects the recording control.

The label must be non-empty, contain no control characters, and fit within 39
bytes. Extra fields are rejected. The button occupies its position in the
device's ordered `button` list and can also be inherited through an Asterisk
configuration template.

More than one recording button may be configured on a device:

```ini
button = feature, Record calls, monitor
button = feature, Record backup, monitor
```

They are mirrored controls, not separate recording destinations. Pressing
either button changes the same device-wide armed/active state, and every
configured recording button is updated together. Each button retains its own
configured label.

In Sorcery mode, use the same button value in an indexed device field:

```json
{
  "fields": [
    {"attribute": "button.0001", "value": "line, 1001, label=Reception"},
    {"attribute": "button.0002", "value": "feature, Record calls, monitor"}
  ]
}
```

### Record soft key

The default soft-key profile does not expose recording. Add `monitor` only to
the connected modes where users should be allowed to see it, then assign that
profile to the device:

```ini
[recording-softkeys]
type = softkey_profile
connected = hold, monitor, end_call, transfer, conference
connected_transfer = hold, monitor, end_call, transfer
connected_conference = hold, monitor, conference_list, end_call

[SEP001122334455]
type = device
softkey_profile = recording-softkeys
button = line, 1001, label=Reception
```

The rest of the profile should contain every soft key required by the site;
these three lines are an example of the modes relevant to recording, not a
complete recommended profile.

A physical recording button is not required for the soft key or AMI action.
Without a physical button, however, there is no off-call arming control or
device-wide recording lamp.

After editing `sccp.conf`, validate and reload it:

```sh
chan-sccp2-config-checker --canonical /etc/asterisk/sccp.conf
asterisk -rx 'sccp reload'
```

Adding, removing, moving, or renaming a recording button changes that device's
station definition. A successful reload reconnects the affected device so the
phone can request the new button template; unrelated devices are not
reconnected.

## Handset behavior

### Physical button

The physical button has two behaviors based on the device's current call:

- With an owned connected or barged call, it starts recording that call. If
  that call already has a driver-owned recording, it stops it instead.
- With no controllable current call, it toggles automatic recording between
  armed and disarmed.

The armed setting is device-wide and is persisted in Asterisk's database. It
survives phone registration, configuration reloads, module reloads, and
Asterisk restarts. It is not a configuration default: a newly configured
button begins disarmed unless that device already has a valid persisted armed
state.

While armed, each call is considered for automatic recording when it becomes
connected or barged. The automatic attempt is one-shot for that PBX call:

- a successful start is not started a second time;
- a failed automatic start is not retried repeatedly on later call events; and
- manually stopping a recording suppresses automatic restart for the rest of
  that call.

The armed setting itself remains on after an automatic start and applies to
future calls. A failed automatic attempt can still be retried manually with the
physical button or `monitor` soft key. To disarm automatic recording with the
physical button, press it when there is no controllable current call. Stopping
the current call's recording does not also disarm future calls.

### Soft key

The `monitor` soft key toggles only the call identified by the phone's current
soft-key event. It does not arm future calls and is accepted only for a call
owned by that device. Starting requires the call to be connected or barged;
an already active recording can still be stopped as the call is winding down.

If a handset start or stop cannot be completed, the phone displays
`Recording unavailable` for four seconds and Asterisk logs the underlying
failure.

### Visual states

Every physical recording button on a device reflects the combination of its
persistent armed setting and whether that device currently owns an active
driver recording:

| State | Meaning | Lamp |
| --- | --- | --- |
| Off | Not armed; no active recording | Off |
| Armed | Future eligible calls are recorded automatically | Steady on |
| Active | A call is recording; future calls are not armed | Wink |
| Armed and active | A call is recording and future calls remain armed | Blink |

While active, the phone also receives the SCCP per-call recording indicator.
The driver appends ` (Recording)` to a physical button's label when the
firmware's label capacity permits it. A missing suffix does not mean the
recording failed; use the lamp, per-call indicator, logs, or output file to
confirm the state.

Phones negotiating protocol version 16 or newer normally receive Cisco's
multistate feature-button projection. Protocol version 15 and older, plus the
Cisco 8941 and 8945, receive the generic feature projection. The driver still
sends the corresponding lamp mode, but exact icons and cadence can vary by
model and firmware.

Muted recordings remain visually active. Mute state is available in the AMI
response but is not a fifth physical-button state.

## Recording files

Handset-initiated and automatically armed recordings use a collision-resistant
WAV basename derived from Asterisk's channel unique ID:

```text
sccp-<sanitized-unique-id>-<sha256-fingerprint>.wav
```

The complete basename is bounded to 128 bytes and contains no directory
separator. Asterisk resolves it beneath its configured monitoring directory.
The unique-ID fingerprint prevents two distinct channel IDs from producing the
same name after unsafe filename characters are replaced or a long ID is
truncated.

AMI starts require an explicit filename. The filename must be 1 to 255 bytes,
may contain only ASCII letters, digits, `.`, `_`, and `-`, must not begin with
`.`, and cannot be `.` or `..`. Directory separators are not accepted, so the
file is also created beneath Asterisk's monitoring directory. Use an extension
supported by the installed Asterisk format modules; `.wav` is the normal
choice.

The driver does not publish recording filenames in its debug output or AMI
result. Operators that need to correlate a file should choose the explicit AMI
filename or inspect the monitoring directory and Asterisk's normal MixMonitor
events.

## AMI control

`SCCPRecording` accepts `start`, `stop`, `mute`, and `unmute`. Its `CallId` is
the driver's positive PBX call identifier, shown as `PBX ID` by `sccp show
channels` and as `PbxCallId` by the `SCCPShowChannels` AMI action. It is not the
handset call ID shown in the adjacent CLI column.

Start a recording with an explicit target:

```text
Action: SCCPRecording
Command: start
CallId: 42
Filename: support-42.wav
Append: no
BridgedOnly: no
```

`Filename` is required for `start`. `Append` and `BridgedOnly` are optional
booleans that default to `no`; they select the corresponding MixMonitor append
and bridged-audio behavior. `Direction` is not accepted for a start.

Stop the driver's recording for the call:

```text
Action: SCCPRecording
Command: stop
CallId: 42
```

`stop` accepts no filename, append, bridged-only, or direction fields. A
successful stop also suppresses an armed automatic restart for that same live
call.

Mute or unmute recorded audio:

```text
Action: SCCPRecording
Command: mute
CallId: 42
Direction: both
```

```text
Action: SCCPRecording
Command: unmute
CallId: 42
Direction: both
```

`Direction` is required for mute operations and accepts `read`, `write`, or
`both`. Asterisk applies the operation to matching MixMonitor audio hooks on
the channel, and the response's `Affected` field reports how many hooks were
changed. Take care when the dialplan has independently installed another
MixMonitor hook on the same channel.

A successful response includes `Command`, `CallId`, `Active`, `Muted`, and
`Affected`. Starting an already owned recording, stopping a missing recording,
using a stale PBX call ID, or supplying fields that do not belong to the chosen
command returns an AMI error. The action is registered under Asterisk's
`system`, `config`, and `reporting` AMI privilege categories. Asterisk permits
the action when an account's write permissions overlap that category mask, so
scope manager accounts according to the site's access policy.

AMI can start a recording without a configured physical button or recording
soft key. When the owning handset is present, it still receives the per-call
recording indicator. If physical recording buttons are configured, they also
mirror the resulting active state.

## Teardown, reload, and removal

The recording session retains the original device and handset call identity
needed to clear its indicators even if the controller's current call view has
already changed. Call teardown, module shutdown, and explicit stop all cause
the driver to retire the owned session and release its media anchor. An
explicit stop reports failure if MixMonitor cannot stop or the original media
path cannot be restored.

Configuration reloads are transactional. An invalid recording button or
soft-key profile is rejected without partially replacing the running
configuration.

Removing the last physical recording button from a device also removes that
device's persisted armed override during successful reconciliation. If a
recording button is added again later, it starts disarmed. Removing the
physical button does not disable AMI or soft-key recording when those controls
remain available.

## Verification and troubleshooting

Use a test device and a non-sensitive call before enabling recording broadly:

1. Confirm `app_mixmonitor.so` is running.
2. Validate `sccp.conf`, run `sccp reload`, and allow the affected phone to
   reconnect.
3. Run `sccp show devices SEP001122334455 buttons` and confirm the recording
   button appears as a feature with target `monitor`.
4. Run `sccp show channels` during a connected test call and note both the PBX
   ID and handset call ID.
5. Press the recording control and confirm the per-call indicator and physical
   lamp, if configured.
6. Confirm a new `.wav` file appears in Asterisk's monitoring directory and
   contains both expected audio directions.
7. Stop recording, then verify the file is closed and the direct-media policy
   returns to its expected state.

If recording does not start:

- Check the Asterisk log for `unable to change SCCP recording state` or
  `unable to start armed SCCP recording`.
- Confirm the call is owned by that handset and has reached `connected` or
  `barged`; ringing, dialing, and held calls cannot start a new handset
  recording.
- Confirm `app_mixmonitor.so` is loaded, the monitoring directory is writable,
  and the filesystem is not full.
- Inspect `sccp show media` and verify that Asterisk can anchor the call's RTP.
- If an armed automatic start already failed once, use the button or soft key
  for a manual retry, or test with a new call. Repeated call events do not retry
  the automatic start.

If the soft key is missing, confirm the device selects the intended
`softkey_profile` and that `monitor` appears in the active connected mode. The
built-in default deliberately omits it.

If the physical button is missing or unchanged, confirm the exact canonical
syntax, inspect the device's button inventory, and verify that the phone
reconnected after the successful reload. A recording label longer than 39
bytes or any fourth field makes the configuration invalid.

If AMI control fails, obtain the PBX ID again from `sccp show channels` or
`SCCPShowChannels`; PBX IDs are live identities and become stale after call
teardown. Confirm that start includes exactly one valid `Filename`, mute and
unmute include exactly one valid `Direction`, and stop includes neither.
