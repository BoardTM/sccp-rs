# sccp-rs -> Cisco SCCP implemented in Rust

This is repo contains three things

- Implementation of the Cisco SCCP protocol in Rust
- Asterisk module for SCCP *(chan_sccp2)* written in Rust
- Simple SCCP<->SIP app to bridge an SCCP phone to SIP

I have some Cisco phones where the SIP firmware is truly ass to tinker with for a variety of reasons. Most people on the internet recommends running these phones on the SCCP firmware rather than the SIP firmware. This however poses the issue of what server one should use to drive the SCCP firmware. You either have to use [Cisco CallManager](https://www.webex.com/us/en/products/suite/enterprise-cloud-calling/CUCM.html), a plugin to Asterisk called [chan_sccp](https://github.com/chan-sccp/chan-sccp). The first one is way outside of my price range and the second one is somewhat abandoned, only functioning on older version of Asterisk (some users reporting having to run this on Asterisk 19). There are some other projects but they are either a full PBX or has some commercial angle to it.

Given I was pursing a project that was going to use SCCP phones, I was somewhat motivated to solve this problem. After working with [PJSIP](https://www.pjsip.org/) for [SIPcord](https://sipcord.net/), I came to really appreciate the way they built a self contained library that implemented the core of what you needed to do SIP. `chan_sccp` implements the protocol but it's very much tied to implementation details of Asterisk.

This project shoots in the same direction as PJSIP by separating the **protocol** and the **asterisk module** into two distinct crates. The protocol crate can easily be reused for other implementations, which is what I'm doing with my small app *(like pjsua)* but I also wanted to validate the implemenation, hence the Asterisk module. It was developed by researching the protocol from [chan_sccp](https://github.com/chan-sccp/chan-sccp) and [mod_skinny](https://github.com/sangoma/freeswitch/tree/master/src/mod/endpoints/mod_skinny), both of which we all owe a tremendous deal for laying the groundwork for making something like this possible.

**AI DISCLAIMER:** I heavily used `gpt-5.6-sol`. This was not a *"make no mistakes, one shot"* ordeal, but rather a very tedious multi-week process with me reviewing protocol implementations and iterating with the model. I'm being as upfront about this as I can, you can spare me the purity testing.

## SCCP Protocol (sccp-protocol)

The [sccp-protocol](https://crates.io/crates/sccp-protocol) crate provides a typed SCCP wire codec and an asynchronous [Tokio](https://tokio.rs/) server for building call-control applications around Cisco SCCP phones. It handles message framing and serialization, registration and session lifecycle, phone provisioning, call and media control, soft keys, QoS, and Cisco IP Phone XML. Applications receive strongly typed events from connected phones and send typed commands through a cloneable server handle.

The crate deliberately contains no SIP or PBX policy. It can be used as the phone-facing layer of an Asterisk channel driver, a protocol analyzer, or a standalone SCCP application such as the bridge in this repository. See the [API documentation](https://docs.rs/sccp-protocol) for a server example and the lower-level message APIs.

## Asterisk Module

In `asterisk-module` you will find a completely new channel driver called `chan_sccp2`, written in Rust. It targets Asterisk 22 and 23; production builds select one of those major-version ABI lanes and reject a different major at load time. *"But wait? Isn't Asterisk written in C?"* Yes, and Rust has great C interop, so that's what we used.

The driver exposes SCCP as a native Asterisk channel and connects the protocol server to Asterisk's dialplan, RTP/media, device state, hints, message waiting, parking, pickup, transfers, conferencing, call forwarding, CLI, AMI, and realtime configuration. It is a new **completely new** implementation built on `sccp-protocol`, not a fork of the old `chan_sccp` module.

You can find the latest [pre-compiled release](https://github.com/coral/sccp-rs/releases) if you'd rather just sling an .so from the internet, if not then the easiest way to produce a Linux x86-64 module is with Docker.

```sh
./asterisk-module/build-linux-x86_64.sh 22
# or
./asterisk-module/build-linux-x86_64.sh 23
```

The artifact is written to `dist/chan_sccp2-asterisk-<major>-linux-x86_64.so`. Install it in Asterisk's module directory as `chan_sccp2.so`, copy `asterisk-module/sccp.conf.example` ( or [copy it from here](https://github.com/coral/sccp-rs/blob/master/asterisk-module/sccp.conf.example)) to Asterisk's configuration directory as `sccp.conf`, and edit the example device and line definitions for your phones.

## SCCP<->SIP app

in progress

## Developing

- Clone the repo
- `git submodule update --init --recursive`
- `cargo build`

## Contributing

PRs welcome

## License

MIT
