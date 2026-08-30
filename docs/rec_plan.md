# SCCP One-Touch and Armed Recording

## Summary

Implement both accepted handset controls while keeping Asterisk responsible for audio capture:

- The connected-call **Record** softkey immediately starts or stops recording for that handset’s call leg.
- A programmable `monitor` feature button controls the current call when one is eligible; otherwise it arms or disarms recording for future calls.
- Continue using Asterisk MixMonitor, which records through an audiohook. Preserve the existing media-anchor transaction because recording cannot work while RTP bypasses Asterisk. [MixMonitor documentation](https://docs.asterisk.org/Asterisk_22_Documentation/API_Documentation/Dialplan_Applications/MixMonitor/), [Asterisk direct-media behavior](https://docs.asterisk.org/Configuration/Interfaces/Asterisk-REST-Interface-ARI/Introduction-to-ARI-and-Bridges/ARI-and-Bridges-Basic-Mixing-Bridges/).

## Handset Behavior and Configuration

- Keep recording opt-in; do not add it to the built-in softkey profile.
- Enable the softkey by adding `monitor` to `connected`, `connected_transfer`, and `connected_conference` in a custom profile.
- Configure the physical control with the canonical syntax:
  `button = feature, Record calls, monitor`
- Softkey presses toggle only the addressed connected or barged call.
- Physical-button behavior:
  - If the current call is recording, stop it without changing the armed state.
  - If the current call is eligible but not recording, start recording without changing the armed state.
  - If no eligible call exists, toggle the persisted device-wide armed state.
- An armed device starts recording once when its next owned call becomes connected or barged.
- Stopping an armed call returns the button to Armed and must not restart recording on that same call; the next eligible call starts automatically.
- Armed state survives registration, phone reconnects, module reloads, and process restarts until explicitly disarmed.
- Recording remains scoped to this handset’s call leg and stops when that leg ends; it does not follow a transferred conversation.
- Provide visual indication only: call-specific SCCP `RecordingStatus` plus physical-button state. Do not add MixMonitor beeps.
- Handset-generated files use a bounded, path-safe basename derived from the Asterisk channel unique ID, stored in Asterisk’s configured monitor directory as WAV. Do not include caller-controlled metadata.

## Implementation Changes

- In `sccp-protocol`, add typed recording-button semantics rather than representing it as an arbitrary generic toggle:
  - `RecordingButtonDefinition`
  - `ButtonDefinition::Recording`
  - `RecordingButtonState::{Off, Armed, Active, ArmedActive}`
  - `DeviceEventKind::RecordingButton`
  - `CommandAction::SetRecordingButtonStatus`
- Accept both generic-feature and multiblink stimuli only for configured recording-button instances.
- Project recording buttons according to station capabilities:
  - Protocol v15 and earlier, plus Cisco 8941/8945: generic feature button with boolean/lamp state.
  - Other v16+ stations: multiblink states `0`, `0x020302`, `0x030203`, and `0x030205`.
  - Use the configured label when off/armed; append ` (Recording)` while active only when it fits the negotiated wire field.
- Allow multiple physical recording buttons on one device; all mirror the same semantic state.
- Add `recording_armed` to device feature state and persist it under a semantic device key rather than a button-instance key. Removing every recording button during reload clears the obsolete persisted override.
- Keep active state exclusively in the live recording registry. Retain device and handset-call ownership metadata with each session so status remains publishable during teardown.
- Route softkeys, physical buttons, automatic armed starts, AMI starts/stops, callbacks, and cleanup through the existing ordered recording transaction.
- Add a private recording-trigger channel owned by the existing serialized event loop. Successful answer, auto-answer, and barge transitions enqueue eligibility once; the recording registry deduplicates attempts per call.
- An armed automatic-start failure:
  - Leaves the device armed.
  - Returns the button to Armed.
  - Displays `Recording unavailable`.
  - Does not retry continuously on the same call.
- Extend the recording provider with typed automatic versus explicitly named targets. The Asterisk adapter generates and sanitizes automatic filenames from the channel unique ID; AMI-provided filenames retain their existing validation.
- Add `app_mixmonitor` as an optional module dependency. If unavailable, the channel driver still loads and recording requests fail cleanly.

## Test Plan

- Protocol tests cover generic and multiblink button templates, both accepted stimuli, exact four-state words, 7925-style modern projection, and the 8941/8945 fallback.
- Configuration tests cover the canonical physical-button syntax, softkey opt-in, multiple mirrored buttons, inheritance, reload, and invalid arguments.
- Persistence tests verify armed-state restore, rollback on storage failure, removal cleanup, and no persistence of active state.
- Runtime tests verify:
  - Softkey start/stop and call-specific status.
  - Physical-button arm/disarm without a call.
  - Automatic start on the next eligible call.
  - Stopping while armed does not restart the same call.
  - Start failure preserves arming.
  - Hangup/disconnect stops MixMonitor and releases the media anchor.
  - AMI and callback-driven changes refresh both call and button indicators.
- Native tests verify unique-ID filename sanitization, basename/path safety, bounded length, and collision-resistant output.
- Run workspace tests and clippy, build both supported Asterisk ABI lanes, and perform handset smoke tests on a 7925 and 8945. The live acceptance test must produce and finalize a nonempty WAV in the monitor directory.

## Assumptions

- Recording authorization, consent, retention, and deletion policy remain deployment responsibilities.
- V1 does not add automatic administrative recording rules, transfer-following, warning tones, format selection, or filename templates.
- Existing AMI recording controls remain supported and are not made dependent on handset configuration.
