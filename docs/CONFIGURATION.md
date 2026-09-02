# Configuring `chan_sccp2` with `sccp.conf`

This is the complete reference for `sccp.conf`, the configuration file read by
the `chan_sccp2` channel driver. It documents every option the module accepts,
its type, its default, and the rules that reject a bad value.

If you are installing the module for the first time, start with
[Installing `chan_sccp2` for Asterisk](INSTALL.md) and come back here when you
need an option that the getting-started walkthrough does not cover.

The module reads `$SCCP_CONFIG` when that environment variable is set, and
otherwise `sccp.conf` in Asterisk's configuration directory.

## File format

`sccp.conf` uses Asterisk's INI dialect: `[section]` headers followed by
`key = value` settings.

```ini
[general]
bind = 0.0.0.0:2000
advertised_ipv4 = 192.0.2.10

[SEP001122334455]
type = device
button = line, 1001

[1001]
type = line
context = from-sccp
```

### Comments and quoting

A `;` starts a comment that runs to the end of the line, unless it appears
inside double quotes. Lines beginning with `#` are directives, not settings.

A value that is wrapped entirely in double quotes is unquoted before use, with
`\"` and `\\` as the two escapes. Quote a value when it has leading or trailing
whitespace you want to keep, when it contains a `;`, or when it starts with `#`:

```ini fragment=sections
[1001]
type = line
callerid = "Reception; front desk" <1001>
```

### Option names

Option names are matched case-insensitively, so `direct_media`, `DIRECT_MEDIA`
and `Direct_Media` are the same option. Punctuation is significant:
`direct-media` is not a spelling of `direct_media`, and it is rejected.

Every option has one **canonical** name — the spelling used throughout this
document and in `sccp.conf.example`. Many options also accept **compatibility
aliases** carried over from other SCCP implementations. Aliases work, but they
are inputs only: the module's canonical output never emits them, and
`chan-sccp2-config-checker --canonical` rejects them. See
[Compatibility aliases](#compatibility-aliases) for the full table.

An unrecognized option name is a hard error, not a warning. So is a valid
option used in the wrong kind of section, such as `permit` in a line section.
Nothing is silently ignored.

### Repeated options

Most options may appear only once per section. A second occurrence is an error,
and so is combining an option with one of its own aliases:

```ini rejected
[general]
sccp_dscp = CS3
sccp_tos = 0x60   ; error: one value (aliases may not be combined)
```

These options are genuinely repeatable and accumulate in the order written:

| Section | Repeatable options |
| --- | --- |
| `[general]` | `deny`, `permit`, `localnet`, `signaling_server`, `allow`, `disallow` |
| device | `deny`, `permit`, `permit_host`, `setvar`, `allow`, `disallow`, `line`, `button`, `feature_default` |
| line | `setvar`, `allow`, `disallow` |

For `deny`, `permit`, `localnet` and `permit_host`, an empty value clears
everything accumulated so far, which is how a device drops a list it inherited
from a template:

```ini fragment=device
deny =                       ; clear the inherited rules
permit = 192.0.2.0/24
```

Three `[general]` options are exempt from the duplicate check and simply take
the last value written: `server_name`, `keepalive` and `secondary_keepalive`.
Line `label`, `context` and `callerid` behave the same way.

### Values

Booleans accept `yes`, `true`, `on` and `1`, or `no`, `false`, `off` and `0`.
Anything else is rejected. `early_media` additionally accepts `none` as a false
value and `offhook`, `immediate`, `dial`, `ringout` and `progress` as true.

Named enumerated values — tones, ring types, NAT modes, DSCP names, codecs,
soft keys, features — ignore case and non-alphanumeric characters. `Inside Dial
Tone`, `insidedialtone` and `INSIDE_DIAL_TONE` are one value, and so are `AF41`
and `af41`. This folding does **not** apply to booleans or to `mwi_lamp`,
`call_answer_order`, `configuration_source` and `fallback`, which are matched
as plain lowercase words.

Lists are comma-separated, except `regcontext` and `regexten`, which use `&`
because their entries may contain commas.

### Includes

The standalone parser expands `#include` and `#tryinclude`. The argument may be
bare, `"quoted"` or `<angled>`, and a relative path resolves against the
directory of the including file. `#tryinclude` tolerates a missing file;
`#include` does not. Includes may nest 32 deep, and a cycle is an error.

```ini fragment=directives
#include "sccp.d/devices.conf"
#tryinclude <sccp.local.conf>
```

Inside a loaded module, Asterisk's own configuration loader performs this
expansion instead, with the same directives.

### Templates and inheritance

A section header may carry a suffix in parentheses. `(!)` marks the section as a
template, which is never instantiated itself. Any other name is a parent
template to inherit from:

```ini fragment=sections
[standard-line](!)
type = line
context = from-sccp
language = en

[1001](standard-line)
label = Reception
callerid = "Reception" <1001>
```

Parents are applied left to right, then the section's own settings. A
non-repeatable option in the child replaces every inherited occurrence; a
repeatable option appends to what it inherited. Replacement is by semantic
identity rather than spelling, so a child's `cfwdall` replaces an inherited
`forward_all_enabled`.

A parent must be a template, the child and parent must be the same `type`, and
cycles are rejected. Templates are supported for device and line sections only —
a soft-key profile may not be a template.

## Section types

| Section | Identified by | Names |
| --- | --- | --- |
| `[general]` | the section name | Server-wide policy and defaults |
| device | `type = device` | `SEP` plus the phone's 12 hex MAC characters |
| line | `type = line` | The logical line number |
| soft-key profile | `type = softkey_profile` | A profile name you choose |

`[general]` is optional and is parsed before everything else regardless of where
it appears in the file, so later sections may refer to its policy. Its name is
matched case-insensitively, and it takes no `type` key.

A device section's name is the device identity the phone presents at
registration: up to 15 ASCII alphanumeric characters, upper-cased. For a phone
with MAC address `00:11:22:33:44:55` the section is `[SEP001122334455]`, and it
must match the device name in the phone's TFTP configuration.

A line section's name is the logical line number that device buttons reference.

A soft-key profile's name is trimmed and lower-cased. The profile named
`default` is built in; defining a `[default]` profile section replaces it.

A file-backed configuration must define at least one device and at least one
line, every device must have at least one line, and every line must be assigned
to some device. A section that is neither `[general]` nor a template must
resolve a `type`, from its own settings or from a parent.

## `[general]` options

Every `[general]` option is optional. Defaults below are what the module uses
when the option is absent.

### Configuration source

| Option | Type / values | Default | Meaning |
| --- | --- | --- | --- |
| `configuration_source` | `file` \| `sorcery` | `file` | Whether devices and lines come from this file or from Asterisk's dynamic configuration API |
| `device_table` | realtime family name | none | Realtime family holding device rows |
| `line_table` | realtime family name | none | Realtime family holding line rows |

`device_table` and `line_table` must be set together, must differ from each
other, and are at most 45 characters of `A-Z`, `a-z`, `0-9` and `_`. They
overlay ordered realtime rows onto this file; a failed query or an invalid
merged candidate leaves the live configuration unchanged. No SQL schema ships
with the module — these are Asterisk realtime family names your deployment
supplies. They cannot be combined with `configuration_source = sorcery`.

### Listeners

| Option | Type / values | Default | Meaning |
| --- | --- | --- | --- |
| `bind` | `address:port` | `0.0.0.0:2000` | Combined clear-listener endpoint |
| `bind_address` | IPv4 or IPv6 address | `0.0.0.0` | Clear-listener address |
| `port` | 1–65535 | `2000` | Clear-listener port |
| `tls_bind` | `address:port` | `0.0.0.0:2443` | Combined TLS-listener endpoint |
| `tls_bind_address` | IPv4 or IPv6 address | `0.0.0.0` | TLS-listener address |
| `tls_port` | 1–65535 | `2443` | TLS-listener port |

`bind` is a shorthand for `bind_address` plus `port`, and the two forms may not
be combined. The same holds for `tls_bind` against `tls_bind_address` and
`tls_port`. The clear and TLS listeners must not resolve to the same socket.

### TLS credentials

| Option | Type / values | Default | Meaning |
| --- | --- | --- | --- |
| `tls_combined_pem` | path | none | One PEM file holding both certificate and private key |
| `tls_certificate` | path | none | Server certificate presented to stations |
| `tls_private_key` | path | none | Private key matching `tls_certificate` |
| `tls_trust_store` | path | none | Trusted certificate authorities for client certificates |

Supply either `tls_combined_pem` on its own, or `tls_certificate` and
`tls_private_key` together. The two forms may not be combined, and
`tls_trust_store` belongs to the split form. TLS policy is validated and
retained, and devices may require it through `transport`, but this build serves
only the clear listener; these settings are ready for the TLS listener.

### Advertised addressing and NAT

| Option | Type / values | Default | Meaning |
| --- | --- | --- | --- |
| `advertised_ipv4` | IPv4 address or `none` | `127.0.0.1` | IPv4 address placed in media and server responses |
| `advertised_ipv6` | IPv6 address or `none` | `none` | IPv6 address placed in media and server responses |
| `advertised_address` | IPv4 or IPv6 address | — | Legacy single-family form of the two above |
| `localnet` | network, repeatable | `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16` | Ranges treated as locally routed |
| `externip` | IP address or `none` | none | Fixed external address for NAT traversal |
| `externhost` | DNS hostname or `none` | none | Hostname resolved to discover the external address |
| `externrefresh` | 1–86400 seconds | `60` | How often `externhost` is re-resolved |
| `nat` | `auto` \| `on` \| `off` \| `(auto)on` \| `(auto)off` | `auto` | Address-rewriting mode for signaling and media |

At least one advertised family must be configured and reachable by the phones,
and neither may be an unspecified address such as `0.0.0.0`. `advertised_address`
sets whichever family it names and clears the other; it may not be combined with
the explicit `advertised_ipv4` and `advertised_ipv6` forms.

Use `externip` or `externhost`, never both. `externrefresh` is meaningful only
with `externhost`.

Networks accept the literal `internal` (expanding to the three RFC 1918 ranges),
CIDR notation such as `192.0.2.0/24` or `2001:db8::/32`, and IPv4
address-with-netmask such as `192.0.2.0/255.255.255.0`.

### Access control

| Option | Type / values | Default | Meaning |
| --- | --- | --- | --- |
| `deny` | network, repeatable | no ACL | Refuse station connections from this range |
| `permit` | network, repeatable | no ACL | Accept station connections from this range |

Rules are evaluated in the order written, so the usual shape is a broad `deny`
followed by the specific `permit` entries. An empty value clears the rules
accumulated so far. Devices may replace this list with their own.

### Quality of service

| Option | Type / values | Default | Meaning |
| --- | --- | --- | --- |
| `sccp_dscp` | DSCP value or name | `24` (`CS3`) | Signaling DSCP |
| `sccp_cos` | 0–7 | `4` | Signaling 802.1p class of service |
| `sccp_tos` | TOS byte | — | Signaling TOS, legacy form of `sccp_dscp` |
| `audio_dscp` | DSCP value or name | `46` (`EF`) | Audio DSCP |
| `audio_cos` | 0–7 | `6` | Audio class of service |
| `audio_tos` | TOS byte | — | Audio TOS, legacy form of `audio_dscp` |
| `video_dscp` | DSCP value or name | `34` (`AF41`) | Video DSCP |
| `video_cos` | 0–7 | `5` | Video class of service |
| `video_tos` | TOS byte | — | Video TOS, legacy form of `video_dscp` |

A DSCP value is `0`–`63` or one of the names listed under
[DSCP names](#dscp-names). A TOS byte is `0`–`255` or `0x00`–`0xff`, or a DSCP
name, and is stored as the byte shifted right by two. Each traffic class has one
DSCP slot, so `sccp_tos` and `sccp_dscp` may not both be set, and likewise for
the audio and video pairs.

### Station presentation

| Option | Type / values | Default | Meaning |
| --- | --- | --- | --- |
| `server_name` | string | `Asterisk SCCP` | Server name presented to stations |
| `dateformat` | date template | `D/M/Y` | How phones render the server-provided date |
| `tzoffset` | −14 to 14 hours | `0` | Station clock offset from UTC |
| `language` | string | `en` | Default Asterisk language, inherited by lines |
| `accountcode` | string | none | Default CDR account code, inherited by lines |
| `ring_type` | ring pattern | `Outside` | Ring pattern for ordinary inbound calls |
| `call_waiting_tone` | tone or `0` | `CallWaiting` | Tone announcing a second call; `0` disables it |
| `call_waiting_interval` | 0–86400 seconds | `0` | Repeat interval for that tone; `0` plays it once |
| `remote_hangup_tone` | tone or `0` | none | Brief tone after the far end clears |
| `autoanswer_ring_time` | seconds | `1` | Alerting delay before an auto-answer call connects |
| `autoanswer_tone` | tone | `Zip` | Tone warning that a call answered automatically |

`dateformat` is at most seven bytes. It must contain `D`, `M` and either `Y` or
`YY` exactly once each, separated by two characters drawn from `/`, `.`, `-` or
space, and may end with `A` to select 12-hour presentation. `tzoffset` is whole
hours and is applied to SCCP time updates as minutes.

`language` is at most 63 printable bytes; `accountcode` is at most 79, and an
empty value means none.

Call-waiting presentations are silent single rings unless `ring_type` is
`Urgent`. See [Tones](#tones) and [Ring types](#ring-types) for the vocabularies.

### Timers and digit collection

| Option | Type / values | Default | Meaning |
| --- | --- | --- | --- |
| `keepalive` | seconds, at least 5 | `30` | Primary station keepalive interval |
| `secondary_keepalive` | seconds, at least 5 | `30` | Alternate keepalive advertised at registration |
| `first_digit_timeout` | 1–86400 seconds | `10` | Wait for the first dialed digit |
| `digit_timeout` | 1–86400 seconds | `5` | Wait between subsequent digits |
| `interdigit_timeout_ms` | 250–86400000 ms | `5000` | The same timer with sub-second precision |
| `digit_timeout_char` | one DTMF character | `#` | Character that ends digit collection immediately |
| `record_digit_timeout_char` | boolean | `no` | Keep that terminator in the dialed number |
| `simulate_enbloc` | boolean | `yes` | Defer routing until the complete number is collected |
| `speed_dial_await_further_digits` | boolean | `no` | Seed collection with a speed dial and wait for more |
| `allow_overlap` | boolean | `no` | Begin routing before the destination is complete |

`digit_timeout` and `interdigit_timeout_ms` are two forms of one timer and may
not both be set. Use the millisecond form when you need sub-second precision.

`digit_timeout_char` is one of `0`–`9`, `*`, `#` or `A`–`D`.

Enable `allow_overlap` only when the dialplan and the downstream provider both
accept overlap DTMF.

### Call handling

| Option | Type / values | Default | Meaning |
| --- | --- | --- | --- |
| `call_answer_order` | `OldestFirst` \| `LastFirst` | `OldestFirst` | Which ringing call the answer action selects |
| `transfer_on_hangup` | boolean | `no` | Hanging up an eligible consultation leg completes its transfer |
| `meetme` | boolean | `yes` | Enable destination-based conference dialing |
| `meetmeopts` | string | `qxd` | Options passed to the conference application |

`meetmeopts` is passed verbatim to the selected conference application. Use
`Mac` for `ConfBridge`.

### Registration and failover

| Option | Type / values | Default | Meaning |
| --- | --- | --- | --- |
| `signaling_server` | server entry, repeatable | none | Advertised primary and failover servers |
| `server_priority` | positive integer | `1` | This server's advertised priority |
| `fallback` | `yes` \| `no` \| `odd` \| `even` | `no` | Whether a phone should move back from another server |
| `backoff_time` | seconds, at least 30 | `60` | Delay after a rejected registration token |
| `regcontext` | `&`-separated contexts | none | Dialplan contexts populated by registrations |

Each `signaling_server` entry is
`priority, name, address, clear-port-or-none, secure-port-or-none`:

```ini fragment=general
signaling_server = 1, Asterisk SCCP, 192.0.2.10, 2000, 2443
signaling_server = 2, Asterisk SCCP Backup, 192.0.2.20, 2000, none
```

The priority is a positive integer and must be unique across entries; the name
is 1–47 characters without control characters; the address must be a specific,
non-multicast address; each port is either a nonzero port or one of `none`,
`off` or `disabled`, and at least one of the two must be a port. At most five
entries are accepted, and when any are configured, `server_priority` must match
one of them.

`regcontext` is at most 79 bytes in total. Each context must be nonempty,
unique, and free of whitespace, `&` and `@`. Lines without an explicit
`regexten` register their logical line number in every listed context.

### Guest hotline

| Option | Type / values | Default | Meaning |
| --- | --- | --- | --- |
| `hotline_enabled` | boolean | `yes` | Let unknown devices register onto a guest hotline |
| `hotline_extension` | destination | `111` | Extension the hotline dials |
| `hotline_context` | string | `default` | Context in which that extension is resolved |
| `hotline_label` | string | `hotline` | Station-facing label for the hotline appearance |

The guest hotline is **enabled by default**. A device that is not configured in
`sccp.conf` can therefore register and reach `111@default` unless you turn it
off. Set `hotline_enabled = no` to require every phone to be configured. When it
is enabled, the extension, context and label must all be present.

### Media

| Option | Type / values | Default | Meaning |
| --- | --- | --- | --- |
| `direct_media` | boolean | `no` | Allow RTP to flow directly between endpoints |
| `early_media` | extended boolean | `yes` | Open media before the call is answered |
| `echocancel` | boolean | `yes` | Default handset echo cancellation |
| `silencesuppression` | boolean | `no` | Default voice-activity transmission |
| `audio_encryption` | encryption policy | `off` | Default SRTP policy for audio channels |
| `allow` | codec list, repeatable | all mapped audio codecs | Add codecs to the default set |
| `disallow` | codec list, repeatable | — | Remove codecs from the default set |

Keep `direct_media = no` while bringing a deployment up: anchoring RTP at
Asterisk makes NAT and firewall problems far easier to diagnose.

With no codec directives, each registered phone advertises its mapped audio
formats and Asterisk chooses the best direct or translated path. Use
`disallow`/`allow` only to restrict that set. See [Codecs](#codecs) and
[Audio encryption](#audio-encryption).

### Jitter buffer

| Option | Type / values | Default | Meaning |
| --- | --- | --- | --- |
| `jb_enable` | boolean | `no` | Enable Asterisk's per-channel jitter buffer |
| `jb_force` | boolean | `no` | Buffer even when it is not otherwise needed |
| `jb_log` | boolean | `no` | Log buffered media frames |
| `jb_max_size` | milliseconds | `200` | Maximum buffering delay |
| `jb_resync_threshold` | milliseconds | `1000` | Discontinuity that triggers a timeline reset |
| `jb_implementation` | `fixed` \| `adaptive` | `fixed` | Buffering algorithm |

Forced mode also suppresses direct RTP.

### Rejected options

| Option | Behavior |
| --- | --- |
| `trust_phone_ip` | Always rejected. Peer addresses are authoritative; remove the setting. |

The name is still recognized so that an upgrade reports a clear migration error
instead of "unknown option".

## Device options

A device section describes one physical phone. `type = device` is required, from
the section itself or from a template, and every device needs at least one line.

Options that also exist in `[general]` default to the general value and override
it for this device only.

### Identity and appearance

| Option | Type / values | Default | Meaning |
| --- | --- | --- | --- |
| `type` | `device` | — | Required section type |
| `description` | string | the device ID | Human-readable description in diagnostics |
| `softkey_profile` | profile name | `default` | Soft keys shown in each call state |
| `use_redial_menu` | boolean | `no` | Redial opens Placed Calls instead of dialing at once |
| `allow_ringin_notification` | boolean | `no` | Show ringing notification for hinted lines |
| `phone_code_page` | `ISO8859-1` \| `ascii` | `ISO8859-1` | Text encoding for legacy display messages |
| `setvar` | `NAME=value`, repeatable | none | Channel variable set on outbound calls |

`softkey_profile` must name a declared profile or the built-in `default`.
Device variables are applied before line variables; a line variable of the same
name replaces the device value on outbound channels. See
[Channel variables](#channel-variables) for the limits.

### Network and transport

| Option | Type / values | Default | Meaning |
| --- | --- | --- | --- |
| `deny` | network, repeatable | the general ACL | Refuse this device from a range |
| `permit` | network, repeatable | the general ACL | Accept this device from a range |
| `permit_host` | hostname, repeatable | none | Permitted signaling hostname |
| `nat` | NAT mode | the general value | Address rewriting for this device |
| `transport` | `clear` \| `tls` \| `either` | `either` | Required signaling transport |

If a device sets any `deny` or `permit`, its list replaces the general ACL
rather than extending it. An empty value clears entries inherited from a
template. `transport = tls` requires complete general TLS credentials.

`transport` also accepts `tcp` for `clear`, `secure` for `tls`, and `any` for
`either`.

### Quality of service

| Option | Type / values | Default | Meaning |
| --- | --- | --- | --- |
| `sccp_dscp` | DSCP value or name | the general value | Signaling DSCP |
| `sccp_cos` | 0–7 | the general value | Signaling class of service |
| `sccp_tos` | TOS byte | — | Legacy form of `sccp_dscp` |
| `audio_dscp` | DSCP value or name | the general value | Audio DSCP |
| `audio_cos` | 0–7 | the general value | Audio class of service |
| `audio_tos` | TOS byte | — | Legacy form of `audio_dscp` |
| `video_dscp` | DSCP value or name | the general value | Video DSCP |
| `video_cos` | 0–7 | the general value | Video class of service |
| `video_tos` | TOS byte | — | Legacy form of `video_dscp` |

A device QoS setting overrides only the traffic class it names. The same
one-slot-per-class rule applies as in `[general]`.

### Call forwarding

| Option | Type / values | Default | Meaning |
| --- | --- | --- | --- |
| `cfwdall` | boolean | `yes` | Enable the forward-all feature |
| `cfwdbusy` | boolean | `yes` | Enable the forward-on-busy feature |
| `cfwdnoanswer` | boolean | `yes` | Enable the forward-on-no-answer feature |
| `forward_all` | destination or `none` | none | Initial forward-all target |
| `forward_busy` | destination or `none` | none | Initial forward-on-busy target |
| `forward_no_answer` | destination or `none` | none | Initial forward-on-no-answer target |
| `forward_no_answer_timeout` | 1–86400 seconds | `30` | No-answer delay before forwarding |

A forwarding destination is at most 23 bytes without control characters.
`none`, `off`, `disabled` and an empty value all mean no destination.

### Do not disturb and privacy

| Option | Type / values | Default | Meaning |
| --- | --- | --- | --- |
| `dnd_feature` | boolean | `yes` | Let the station show and change DND |
| `dnd` | `off` \| `silent` \| `reject` | `off` | Initial DND mode |
| `privacy_feature` | boolean | `yes` | Let the station show and change privacy |
| `privacy` | boolean | `no` | Whether calls begin with privacy enabled |

`dnd` also accepts `none` and `disabled` for `off`, and `busy`, which is
normalized to `reject`.

### Message waiting

| Option | Type / values | Default | Meaning |
| --- | --- | --- | --- |
| `mwi_lamp` | `off` \| `on` \| `wink` \| `flash` \| `blink` | `on` | Message-waiting lamp mode |
| `mwi_on_call` | boolean | `no` | Keep the indicator visible during a call |

### Parking and conferencing

| Option | Type / values | Default | Meaning |
| --- | --- | --- | --- |
| `park` | boolean | `yes` | Allow parking from this device |
| `conf_allow` | boolean | `yes` | Allow creating and controlling conferences |
| `conf_music_on_hold_class` | string | `default` | Music played to held participants |
| `conf_play_general_announce` | boolean | `yes` | Play room-level conference prompts |
| `conf_play_part_announce` | boolean | `yes` | Play join and leave prompts |
| `conf_mute_on_entry` | boolean | `no` | Join conferences muted |
| `conf_show_conflist` | boolean | `yes` | Offer the participant list |
| `meetme` | boolean | the general value | Enable destination-based conference dialing |
| `meetmeopts` | string | the general value | Options passed to the conference application |

### Media

| Option | Type / values | Default | Meaning |
| --- | --- | --- | --- |
| `direct_media` | boolean | the general value | Allow direct RTP for this device |
| `early_media` | extended boolean | the general value | Open media before answer |
| `force_dtmf_mode` | `auto` \| `rfc2833` \| `skinny` | `auto` | Force a DTMF transport |
| `audio_encryption` | encryption policy | the general value | SRTP policy for this device |
| `allow` | codec list, repeatable | the general set | Add codecs for this device |
| `disallow` | codec list, repeatable | — | Remove codecs for this device |
| `allow_overlap` | boolean | the general value | Overlap dialing for this device |

### Buttons

| Option | Type / values | Default | Meaning |
| --- | --- | --- | --- |
| `button` | button definition, repeatable | — | A provisioned station button |
| `line` | line name with options, repeatable | — | Shorthand for a line button |
| `feature_default` | `instance, boolean`, repeatable | all off | Initial state of a feature button |

Buttons are provisioned in the order written, and `line` keeps its position
relative to surrounding `button` entries. `feature_default` addresses feature
instances, not physical button positions — see
[Feature button arguments](#feature-button-arguments) for how those are counted.
See [Buttons](#buttons) for the full grammar.

### Rejected options

| Option | Behavior |
| --- | --- |
| `trust_phone_ip` | Always rejected. Peer addresses are authoritative; remove the setting. |
| `obsolete_dtmf_mode` | Always rejected. Use `force_dtmf_mode` instead. |

The `obsolete_dtmf_mode` name exists to catch the old `dtmfmode` spelling and
point at its replacement.

## Line options

A line section describes one logical line that device buttons reference.
`type = line` is required, from the section itself or from a template.

### Identity

| Option | Type / values | Default | Meaning |
| --- | --- | --- | --- |
| `type` | `line` | — | Required section type |
| `label` | string | the section name | Station-facing line label |
| `context` | string | `from-sccp` | Dialplan context for calls from this line |
| `callerid` | `"Name" <number>` | the section name | Outbound caller identity |
| `language` | string | the general value | Asterisk language for this line |
| `accountcode` | string | the general value | CDR account code |
| `incoming_limit` | 0–255 | `6` | Concurrent inbound PBX calls allowed |
| `setvar` | `NAME=value`, repeatable | none | Channel variable on inbound and outbound calls |

### Voicemail

| Option | Type / values | Default | Meaning |
| --- | --- | --- | --- |
| `mailbox` | `mailbox` or `mailbox@context` | none | Mailbox driving the message-waiting indicator |
| `voicemail_number` | destination | none | Number dialed by the Messages key |
| `voicemail_transfer` | destination | none | Destination for transfer to voicemail |

A mailbox has no whitespace and at most one `@`, with both parts nonempty. For
the destinations, `none`, `off`, `disabled` and an empty value all mean unset.

### Pickup and groups

| Option | Type / values | Default | Meaning |
| --- | --- | --- | --- |
| `call_group` | numbers and ranges | none | Numeric call groups |
| `pickup_group` | numbers and ranges | none | Numeric pickup groups |
| `named_call_group` | names | none | Named call groups |
| `named_pickup_group` | names | none | Named pickup groups |
| `directed_pickup` | boolean | `yes` | Allow directed pickup of this line |
| `directed_pickup_context` | string | none | Context searched for directed pickup |
| `pickup_mode_answer` | boolean | `yes` | Answer directly instead of presenting the call |

Numeric groups are a comma-separated list of values `0`–`63` and ascending
ranges, such as `1, 3-4`. Entries must be unique.

### Parking and conferencing

| Option | Type / values | Default | Meaning |
| --- | --- | --- | --- |
| `parkinglot` | string | none | Parking lot used by this line |
| `meetme` | boolean | unset | Enable conference dialing on this line |
| `meetmenum` | string | none | Conference destination number |
| `meetmeopts` | string | unset | Options passed to the conference application |

Line `meetme` is tri-state. Leaving it unset inherits the device and general
policy. Setting `meetme = yes` requires `meetmenum`, and `meetme = no` may not
be combined with `meetmenum` or `meetmeopts`.

### Dial tones and hotline

| Option | Type / values | Default | Meaning |
| --- | --- | --- | --- |
| `initial_dialtone_tone` | tone | `Inside Dial Tone` | Tone played at off-hook |
| `secondary_dialtone_digits` | up to 9 DTMF characters | none | Prefix that switches the dial tone |
| `secondary_dialtone_tone` | tone | `Outside Dial Tone` | Tone started on an exact prefix match |
| `adhoc_number` | destination | none | Hotline destination dialed for this line |

`secondary_dialtone_digits` accepts `0`–`9`, `*`, `#` and `A`–`D`.

### Mobility and registration

| Option | Type / values | Default | Meaning |
| --- | --- | --- | --- |
| `pin` | 1–7 digits | none | Mobility login PIN |
| `regexten` | `&`-separated extensions | the line name | Registration extensions |

A `regexten` entry is an extension, optionally `extension@context` to place just
that entry in a specific context. Entries are unique, each at most 79 bytes
without whitespace, `&` or `@`, and 255 bytes in total. `regexten` requires a
nonempty general `regcontext`, and the resolved targets must be unique across
all lines. PINs are redacted from diagnostics.

### Media

| Option | Type / values | Default | Meaning |
| --- | --- | --- | --- |
| `video_mode` | `off` \| `user` \| `auto` | `auto` | Video negotiation for this line |
| `echocancel` | boolean | the general value | Echo cancellation |
| `silencesuppression` | boolean | the general value | Silence suppression |
| `audio_encryption` | encryption policy | the general value | SRTP policy for this line |
| `allow` | codec list, repeatable | the general set | Add codecs for this line |
| `disallow` | codec list, repeatable | — | Remove codecs for this line |

Codec policy resolves line first, then device, then general.

## Buttons

Buttons are declared on a device, one per `button` entry, in the order they
appear on the phone. A device supports 256 logical buttons including addon
expansion.

```ini fragment=device
button = line, 1001, label=Main desk, ring=normal
button = speed_dial, Helpdesk, 2000
button = blf, Warehouse, 2001, 2001@from-internal
button = feature, Do not disturb, dnd, silent
button = service, Directory, http://pbx.example.test/sccp/directory
button = addon, 1, 7914
button = empty
```

| Kind | Form |
| --- | --- |
| `line` | `line, <line>[, option=value …]` |
| `speed_dial` | `speed_dial, <label>, <number>` |
| `speed_dial` with presence | `speed_dial, <label>, <number>, <extension@context>` |
| `blf` | `blf, <label>, <number>, <extension@context>` |
| `feature` | `feature, <label>, <feature>[, argument …]` |
| `service` | `service, <label>, <url>` |
| `addon` | `addon, <slot>, <type>` |
| `empty` | `empty` |

The hint target of a `blf` button is an Asterisk dialplan hint written as
`extension@context`. Its devices may be PJSIP, SIP, SCCP, Custom or any
aggregate Asterisk supports — write the hint, not a channel such as
`PJSIP/foo`.

An `addon` slot is `1`–`56`. See [Addon types](#addon-types).

### Line button options

A `line` button, and the `line =` shorthand, accept these trailing options:

| Option | Value | Meaning |
| --- | --- | --- |
| `label` | text | Button label, overriding the line's own |
| `caller_name` | text | Caller name used for this appearance |
| `caller_number` | text | Caller number used for this appearance |
| `ring` | `normal` \| `silent` \| `disabled` \| `off` | Per-device ringing policy |
| `subscription` | text | Shared-line subscription identity |
| `privacy` | boolean | Per-appearance privacy policy |

```ini fragment=device
button = line, 1001, label=Main desk, caller_name=Reception, caller_number=1001, ring=normal, subscription=1001@from-internal, privacy=no
line = 1002
```

An option key may not repeat within one button.

### Feature button arguments

A `feature` button names one of the [feature names](#feature-names) and may take
arguments:

- A `dnd` button takes `silent`, `reject` or `busy` (normalized to `reject`) to
  fix its mode. Omit the argument to cycle off, reject and silent.
- A `parkinglot` button takes `<lot>` and optionally `RetrieveSingle` or
  `AlwaysShowMenu`, defaulting to `default, RetrieveSingle`. `RetrieveSingle`
  retrieves the only parked call directly and shows the lot menu otherwise;
  `AlwaysShowMenu` forces the menu even for a single call.
- A `monitor` button is a recording button rather than an ordinary feature
  button, and takes no argument. See [Call recording](RECORDING.md).

`feature_default` sets the initial state of a feature button by instance:

```ini fragment=device
button = feature, Do not disturb, dnd, silent
button = feature, Forward all, forward_all
feature_default = 1, off
```

Instances are counted per button kind, not by physical position, so the first
`feature` button on a device is instance 1 regardless of how many line or
speed-dial buttons precede it. Two kinds share the feature counter: `blf`
buttons, and the four-field `speed_dial` form with a hint. Those consume a
feature instance but cannot be addressed by `feature_default`, so a device
with a `blf` button ahead of its feature buttons starts numbering them at 2:

```ini fragment=device
button = blf, Warehouse, 2001, 2001@from-internal
button = feature, Do not disturb, dnd, silent
feature_default = 2, off
```

## Soft-key profiles

A soft-key profile names the actions offered in each call state. Assign one to a
device with `softkey_profile`.

```ini fragment=sections
[reception-softkeys]
type = softkey_profile
on_hook = redial, new_call
connected = hold, end_call, transfer, conference, park
ring_in = answer, immediate_divert, end_call
empty =
```

Each mode takes an ordered, comma-separated list of at most 16 actions, and
duplicates within a mode are rejected. An empty value disables soft keys in that
mode — and so does omitting the mode, because a custom profile does not inherit
the built-in defaults. List every mode you want populated.

| Mode | When it applies |
| --- | --- |
| `on_hook` | Idle, handset down |
| `off_hook` | Handset lifted, no digits yet |
| `digits_following` | Digits are being collected |
| `ring_out` | Outbound call is alerting |
| `ring_in` | Inbound call is alerting |
| `connected` | Call is connected |
| `on_hold` | Call is held |
| `connected_transfer` | Connected during a transfer |
| `connected_conference` | Connected in a conference |
| `hold_conference` | Conference leg is held |
| `off_hook_feature` | Off-hook with a feature active |
| `in_use_hint` | A monitored line is in use |
| `on_hook_stealable` | Idle with a stealable call available |
| `empty` | No call reference |

### Soft-key actions

`answer`, `backspace`, `barge`, `callback`, `conference`, `conference_list`,
`dial`, `direct_transfer`, `do_not_disturb`, `empty`, `end_call`,
`forward_all`, `forward_busy`, `forward_no_answer`, `group_pickup`, `hold`,
`immediate_divert`, `info`, `intercept`, `join`, `meetme`, `monitor`,
`new_call`, `park`, `pickup`, `private`, `redial`, `resume`, `select`,
`transfer`, `transfer_to_voicemail`, `video_mode`.

`dnd` is accepted for `do_not_disturb`, and `cfwdall`, `cfwdbusy` and
`cfwdnoanswer` for the three forwarding actions.

## Value vocabularies

### Tones

Tone options accept a tone name or a numeric value `0`–`255`, also written as
`0x00`–`0xff`. Common names are `Inside Dial Tone`, `Outside Dial Tone`,
`CallWaiting`, `Zip`, `Busy Tone`, `Reorder Tone`, `Alerting Tone` and
`No Tone`. Name matching ignores case and punctuation, so `insidedialtone`
works. Where the table above says `0` disables the tone, a numeric zero is the
disable form.

### Ring types

`Off`, `Inside`, `Outside`, `Feature`, `Silent`, `Urgent`, `Bellcore1`,
`Bellcore2`, `Bellcore3`, `Bellcore4`, `Bellcore5`.

### DSCP names

`CS0` through `CS7`, `AF11`, `AF12`, `AF13`, `AF21`, `AF22`, `AF23`, `AF31`,
`AF32`, `AF33`, `AF41`, `AF42`, `AF43`, `EF`, `none`, and the legacy precedence
names `lowdelay`, `throughput`, `reliability` and `mincost`.

### NAT modes

`auto`, `on`, `off`, `(auto)on`, `(auto)off`. Punctuation is ignored, so
`autooff` and `auto-off` are the same as `(auto)off`.

### Feature names

`redial`, `hold`, `transfer`, `forward_all`, `forward_busy`,
`forward_no_answer`, `video`, `voicemail`, `answer_release`, `auto_answer`,
`select`, `feature`, `malicious_call`, `meetme`, `conference`, `park`,
`pickup`, `group_pickup`, `mobility`, `dnd`, `conference_list`,
`remove_last_participant`, `quality_report`, `callback`, `other_pickup`,
`video_mode`, `new_call`, `end_call`, `hunt_group_login`, `queue`,
`parkinglot`, `messages`, `directory`, `application`, `headset`,
`echo_cancellation`.

`last_number_redial` is accepted for `redial`, `cfwdall`, `cfwdbusy` and
`cfwdnoanswer` for the forwarding features, `call_park` for `park`,
`call_pickup` for `pickup`, `group_call_pickup` for `group_pickup`,
`do_not_disturb` for `dnd`, `meetme_conference` for `meetme`, `queuing` for
`queue`, `quality_report_tool` for `quality_report`, and
`acoustic_echo_cancellation` for `echo_cancellation`.

### Addon types

`7914`, `791512`, `791524`, `791612`, `791624`, `spa500s`, `spa500ds`,
`spa932ds`.

Each type also accepts a `cisco` or `addon` prefix, so `cisco7914` and
`addon7914` name the same module as `7914`.

### Codecs

`allow` and `disallow` take a comma-separated codec list. A token may be
prefixed with `!` to invert the directive for that token. `all` may only appear
on its own, and `disallow = all` clears the set:

```ini fragment=line
disallow = all
allow = ulaw, alaw, g722
```

Audio codecs include `ulaw`, `alaw`, `gsm`, `g722`, `g7221`, `g723`, `g726`,
`g728`, `g729`, `ilbc`, `isac`, `opus`, `slin16` and `amr`/`amrwb`. Video
codecs include `h261`, `h263`, `h264` and `h265`. At most 32 preferences are
accepted, at least one audio codec must remain, and every audio codec must have
an Asterisk format mapping.

### Audio encryption

`audio_encryption` takes `off`, or `optional` or `required` followed by one or
more SRTP profiles:

```ini fragment=line
audio_encryption = required, aes-128-hmac-sha1-80
```

Profiles are `aes-128-hmac-sha1-32`, `aes-128-hmac-sha1-80`,
`f8-128-hmac-sha1-32`, `f8-128-hmac-sha1-80`, `aead-aes-128-gcm` and
`aead-aes-256-gcm`. `off` takes no profiles; `optional` and `required` need at
least one. Protected media is validated policy, not yet a runtime transport.

### Channel variables

`setvar = NAME=value` sets an Asterisk channel variable. A name matches
`[A-Za-z_][A-Za-z0-9_]*`, is at most 79 bytes, and must not read as a
credential — names containing `password`, `passwd`, `secret`, `token`,
`authorization` or `credential` are rejected. A value is nonempty and at most
1024 bytes. A section may set at most 32 variables totalling 8192 bytes, and
names must be unique within the section. Values are redacted from diagnostics.

## Examples

### A minimal LAN PBX

One phone, one line, everything else defaulted.

```ini
[general]
bind = 0.0.0.0:2000
advertised_ipv4 = 192.168.10.20
server_name = pbx.example.com
keepalive = 60
direct_media = no
hotline_enabled = no

[SEP00A1B2C3D4E5]
type = device
description = Reception 7961G
button = line, 1001

[1001]
type = line
label = Reception
context = from-sccp
callerid = "Reception" <1001>
mailbox = 1001@default
```

`hotline_enabled = no` is deliberate: the guest hotline is on by default, and
turning it off means only configured phones can register.

### Phones behind NAT

Anchor media, declare which ranges are local, and advertise the public address.

```ini
[general]
bind = 0.0.0.0:2000
advertised_ipv4 = 192.168.10.20
server_name = pbx.example.com
localnet = internal
localnet = 2001:db8:100::/64
externhost = pbx.example.test
externrefresh = 60
nat = auto
direct_media = no
early_media = yes

[SEP00A1B2C3D4E5]
type = device
nat = auto
button = line, 1001

[1001]
type = line
context = from-sccp
callerid = "Reception" <1001>
```

Use `externip` instead of `externhost` when the public address is static.
`externrefresh` applies only to the hostname form.

### TLS credentials and per-device transport

```ini
[general]
bind = 0.0.0.0:2000
advertised_ipv4 = 192.0.2.10
tls_bind = 0.0.0.0:2443
tls_certificate = /etc/asterisk/tls/server.crt
tls_private_key = /etc/asterisk/tls/server.key
tls_trust_store = /etc/asterisk/tls/ca.pem

[SEP00A1B2C3D4E5]
type = device
transport = tls
button = line, 1001

[SEP00A1B2C3D4E6]
type = device
transport = either
button = line, 1002

[1001]
type = line
context = from-sccp

[1002]
type = line
context = from-sccp
```

Replace the split certificate and key with a single `tls_combined_pem` if your
PEM file holds both. This build serves only the clear listener; the TLS policy
is validated and retained for it.

### Templates and a shared line

Two phones share line `1001` and each keep a private line.

```ini
[general]
bind = 0.0.0.0:2000
advertised_ipv4 = 192.0.2.10

[standard-device](!)
type = device
softkey_profile = default
nat = auto
mwi_lamp = on
setvar = ENDPOINT_CLASS=desk

[standard-line](!)
type = line
context = from-sccp
language = en
directed_pickup = yes
pickup_mode_answer = yes

[SEP001122334455](standard-device)
description = Reception front
button = line, 1001, label=Shared, subscription=1001@from-internal
button = line, 1002, label=Private

[SEP001122334456](standard-device)
description = Reception back
button = line, 1001, label=Shared, subscription=1001@from-internal
button = line, 1003, label=Private

[1001](standard-line)
label = Shared
callerid = "Reception" <1001>
incoming_limit = 6

[1002](standard-line)
label = Front private
callerid = "Front" <1002>

[1003](standard-line)
label = Back private
callerid = "Back" <1003>
```

### A full button and soft-key layout

A 7961 with a 7914 addon, custom soft keys, and a mix of button kinds.

```ini
[general]
bind = 0.0.0.0:2000
advertised_ipv4 = 192.0.2.10

[reception-softkeys]
type = softkey_profile
on_hook = redial, new_call, forward_all, pickup, group_pickup, dnd
off_hook = redial, end_call
digits_following = backspace, end_call, dial
ring_out = callback, end_call
ring_in = answer, immediate_divert, end_call
connected = hold, end_call, transfer, conference, park, private
on_hold = resume, new_call, end_call, transfer, conference_list, select, direct_transfer
connected_transfer = hold, end_call, transfer, conference, park, select, direct_transfer
connected_conference = hold, conference_list, join, end_call
hold_conference = resume, new_call, end_call
off_hook_feature = resume, new_call, end_call
in_use_hint = barge
on_hook_stealable = intercept, new_call
empty =

[SEP001122334455]
type = device
description = Reception 7961G
softkey_profile = reception-softkeys
button = line, 1001, label=Main desk, caller_name=Reception, caller_number=1001, ring=normal, privacy=no
button = speed_dial, Helpdesk, 2000
button = blf, Warehouse, 2001, 2001@from-internal
button = feature, Do not disturb, dnd, silent
button = feature, Parked calls, parkinglot, default, RetrieveSingle
button = feature, Forward all, forward_all
button = service, Directory, http://pbx.example.test/sccp/directory
button = addon, 1, 7914
button = empty
feature_default = 2, off

[1001]
type = line
label = Reception
context = from-sccp
callerid = "Reception" <1001>
```

### Voicemail, pickup, parking and conferencing

```ini
[general]
bind = 0.0.0.0:2000
advertised_ipv4 = 192.0.2.10
meetme = yes
meetmeopts = Mac
regcontext = sccp-registrations

[SEP001122334455]
type = device
description = Reception
park = yes
conf_allow = yes
conf_music_on_hold_class = default
conf_mute_on_entry = no
mwi_lamp = on
mwi_on_call = no
button = line, 1001
button = feature, Parked calls, parkinglot, default, AlwaysShowMenu

[1001]
type = line
label = Reception
context = from-sccp
callerid = "Reception" <1001>
mailbox = 1001@default
voicemail_number = 600
voicemail_transfer = 61001
call_group = 1
pickup_group = 1, 3-4
named_call_group = reception
named_pickup_group = front-desk
directed_pickup = yes
directed_pickup_context = from-sccp
parkinglot = default
meetme = yes
meetmenum = 700
meetmeopts = Mac
regexten = 1001&91001
```

### Restricting codecs and requiring encryption

```ini
[general]
bind = 0.0.0.0:2000
advertised_ipv4 = 192.0.2.10
disallow = all
allow = ulaw, alaw, g722

[SEP001122334455]
type = device
audio_encryption = required, aes-128-hmac-sha1-80
button = line, 1001

[1001]
type = line
context = from-sccp
disallow = all
allow = ulaw
video_mode = off
```

### Realtime and Sorcery

Overlay ordered realtime rows onto this file:

```ini
[general]
bind = 0.0.0.0:2000
advertised_ipv4 = 192.0.2.10
device_table = sccp_devices
line_table = sccp_lines

[SEP001122334455]
type = device
button = line, 1001

[1001]
type = line
context = from-sccp
```

Or hand devices and lines to Asterisk's dynamic configuration API entirely,
keeping general policy and soft-key profiles here:

```ini
[general]
configuration_source = sorcery
bind = 0.0.0.0:2000
advertised_ipv4 = 192.0.2.10

[reception-softkeys]
type = softkey_profile
on_hook = redial, new_call
connected = hold, end_call, transfer
ring_in = answer, end_call
empty =
```

Realtime rows and Sorcery objects use the same option names as this file. See
[Dynamic SCCP configuration through ARI](DYNAMIC_CONFIGURATION.md) for the
AstDB mapping, the HTTP calls, and the `name.0004` suffix that orders repeated
options.

## Validating and reloading

Validate a file before loading it. The checker runs the module's own parser and
needs no running Asterisk:

```sh
chan-sccp2-config-checker /etc/asterisk/sccp.conf
chan-sccp2-config-checker --canonical /etc/asterisk/sccp.conf
chan-sccp2-config-checker normalize /etc/asterisk/sccp.conf > /tmp/sccp.canonical.conf
```

Plain validation follows Asterisk's case-insensitive option lookup and accepts
the compatibility aliases below. `--canonical` additionally requires the
canonical spelling of every option. `normalize` writes a deterministic,
template-expanded configuration to standard output and never modifies the
source file.

| Exit code | Meaning |
| --- | --- |
| `0` | The configuration is valid |
| `1` | The configuration is invalid |
| `2` | Bad arguments, or the file could not be read |

Diagnostics name the location as `line N [section].option`, and values of
sensitive options such as `pin`, `setvar` and the TLS key paths print as
`<redacted>`.

After editing `sccp.conf`, apply it with the driver's own reload:

```text
asterisk -rx "sccp reload"
```

A reload builds a complete replacement configuration and commits it only if
every part validates, so a rejected file leaves the running configuration
untouched.

## Compatibility aliases

These spellings are accepted as input and mean exactly the canonical option
listed beside them. Canonical output never emits an alias, and
`chan-sccp2-config-checker --canonical` rejects one. An option and its own alias
may not both appear in the same section.

### `[general]` aliases

| Canonical | Accepted aliases |
| --- | --- |
| `bind` | `clearbind` |
| `bind_address` | `bindaddr`, `clearbindaddr` |
| `port` | `clearport` |
| `advertised_ipv4` | `advertisedaddressipv4` |
| `advertised_ipv6` | `advertisedaddressipv6` |
| `tls_bind` | `securebind` |
| `tls_bind_address` | `secbindaddr`, `tlsbindaddr` |
| `tls_port` | `secport`, `tlsport` |
| `tls_combined_pem` | `certfile`, `tlscombinedpem` |
| `tls_certificate` | `tlscertificatefile` |
| `tls_private_key` | `tlsprivatekeyfile` |
| `tls_trust_store` | `tlscafile` |
| `externip` | `externaladdress` |
| `externhost` | `externalhost` |
| `externrefresh` | `externalrefresh` |
| `sccp_tos` | `signalingtos` |
| `sccp_dscp` | `sccpdscp`, `signalingdscp`, `signaling_dscp` |
| `sccp_cos` | `signalingcos`, `signaling_cos` |
| `audio_tos` | `audiotos` |
| `audio_dscp` | `audiodscp` |
| `audio_cos` | `audiocos` |
| `video_tos` | `videotos` |
| `video_dscp` | `videodscp` |
| `video_cos` | `videocos` |
| `trust_phone_ip` | `trustphoneip` |
| `server_name` | `servername` |
| `first_digit_timeout` | `firstdigittimeout` |
| `digit_timeout` | `digittimeout` |
| `digit_timeout_char` | `digittimeoutchar` |
| `record_digit_timeout_char` | `recorddigittimeoutchar` |
| `speed_dial_await_further_digits` | `speeddialawaitfurtherdigits` |
| `allow_overlap` | `allowoverlap` |
| `call_answer_order` | `callanswerorder` |
| `ring_type` | `ringtype` |
| `call_waiting_tone` | `callwaitingtone` |
| `call_waiting_interval` | `callwaitinginterval` |
| `autoanswer_ring_time` | `autoanswerringtime` |
| `autoanswer_tone` | `autoanswertone` |
| `remote_hangup_tone` | `remotehangup_tone` |
| `hotline_enabled` | `hotlineenabled` |
| `hotline_extension` | `hotlineextension` |
| `hotline_context` | `hotlinecontext` |
| `hotline_label` | `hotlinelabel` |
| `direct_media` | `directrtp` |
| `early_media` | `earlyrtp` |
| `audio_encryption` | `audioencryption` |
| `jb_enable` | `jbenable` |
| `jb_force` | `jbforce` |
| `jb_log` | `jblog` |
| `jb_max_size` | `jbmaxsize` |
| `jb_resync_threshold` | `jbresyncthreshold` |
| `jb_implementation` | `jbimpl` |
| `device_table` | `devicetable` |
| `line_table` | `linetable` |

### Device aliases

| Canonical | Accepted aliases |
| --- | --- |
| `softkey_profile` | `softkeyprofile` |
| `cfwdall` | `forwardallenabled`, `forward_all_enabled` |
| `cfwdbusy` | `forwardbusyenabled`, `forward_busy_enabled` |
| `cfwdnoanswer` | `forwardnoanswerenabled`, `forward_no_answer_enabled` |
| `forward_no_answer_timeout` | `cfwdnoanswertimeout`, `forwardnoanswertimeout` |
| `dnd_feature` | `dndfeature` |
| `privacy_feature` | `private`, `privacyfeature` |
| `feature_default` | `featuredefault` |
| `conf_allow` | `confallow`, `conference_allow` |
| `conf_music_on_hold_class` | `confmusiconholdclass`, `conference_music_on_hold_class` |
| `conf_play_general_announce` | `confplaygeneralannounce`, `conference_play_general_announce` |
| `conf_play_part_announce` | `confplaypartannounce`, `conference_play_participant_announce` |
| `conf_mute_on_entry` | `confmuteonentry`, `conference_mute_on_entry` |
| `conf_show_conflist` | `confshowconflist`, `conference_show_list` |
| `use_redial_menu` | `useredialmenu` |
| `allow_ringin_notification` | `allowringinnotification` |
| `mwi_lamp` | `mwilamp` |
| `mwi_on_call` | `mwioncall` |
| `phone_code_page` | `phonecodepage` |
| `allow_overlap` | `allowoverlap` |
| `force_dtmf_mode` | `forcedtmfmode`, `force_dtmfmode` |
| `direct_media` | `directrtp` |
| `early_media` | `earlyrtp` |
| `audio_encryption` | `audioencryption` |
| `permit_host` | `permithost` |
| `transport` | `transportrequirement`, `transport_requirement` |
| `sccp_tos` | `signalingtos` |
| `sccp_dscp` | `sccpdscp`, `signalingdscp`, `signaling_dscp` |
| `sccp_cos` | `signalingcos`, `signaling_cos` |
| `audio_tos` | `audiotos` |
| `audio_dscp` | `audiodscp` |
| `audio_cos` | `audiocos` |
| `video_tos` | `videotos` |
| `video_dscp` | `videodscp` |
| `video_cos` | `videocos` |
| `trust_phone_ip` | `trustphoneip` |
| `obsolete_dtmf_mode` | `dtmfmode` |

### Line aliases

| Canonical | Accepted aliases |
| --- | --- |
| `incoming_limit` | `incominglimit` |
| `voicemail_number` | `vmnum`, `voicemailnumber` |
| `voicemail_transfer` | `trnsfvm`, `voicemailtransfer`, `transfertovoicemail` |
| `call_group` | `callgroup` |
| `pickup_group` | `pickupgroup` |
| `named_call_group` | `namedcallgroup` |
| `named_pickup_group` | `namedpickupgroup` |
| `directed_pickup` | `directedpickup` |
| `directed_pickup_context` | `directedpickupcontext` |
| `pickup_mode_answer` | `pickupmodeanswer`, `directedpickupmodeanswer` |
| `adhoc_number` | `adhocnumber` |
| `video_mode` | `videomode` |
| `audio_encryption` | `audioencryption` |
