# sccp-rs -> "Skinny Client Control Protocol" implemented in Rust

This is repo contains three things

- Implementation of the Cisco SCCP protocol in Rust
- Asterisk module for SCCP *(chan_sccp2)* written in Rust
- Simple SCCP<->SIP app to bridge an SCCP phone to SIP

We got some Cisco phones where the SIP firmware is truly ass to tinker with and most people on the internet recommends running these phones on the SCCP firmware. That however isn't as clear cut as it should be, you either have to use [Cisco CallManager](https://www.webex.com/us/en/products/suite/enterprise-cloud-calling/CUCM.html) or an plugin to Asterisk called [chan_sccp](https://github.com/chan-sccp/chan-sccp). The first one is way outside of my price range and the second one is somewhat abandoned, only functioning on older version of Asterisk (some users reporting having to run this on Asterisk 14...)

Pursuing a project where we wanted to use SCCP phones, without using an ancinent verison of SCCP seemed somewhat impossible. After working with [PJSIP](https://www.pjsip.org/) for [SIPcord](https://sipcord.net/), I came to really appreciate the way they built a self contained library that implemented the core of what you needed to do SIP. `chan_sccp` implements the protocol but it's very much tied to implementation details of Asterisk.

This aims to solve this, by separating the **protocol** and the **asterisk module** into two distinct crates. The protocol crate can easily be reused for other implementations, which is what I'm doing with my small app but I also wanted to validate the implemenation, hence the Asterisk module.

This was developed by researching the protocol from [chan_sccp](https://github.com/chan-sccp/chan-sccp) and [mod_skinny](https://github.com/sangoma/freeswitch/tree/master/src/mod/endpoints/mod_skinny), both of which we all owe a tremendous deal for laying the groundwork for making something like this possible.

**AI DISCLAIMER:** I heavily used `gpt-5.6-sol`. You may call it whatever you want but this was not a *"make no mistakes"* one shot ordeal, but rather a very tedious multi-week process with me reviewing protocol implementations and iterating with the model.

## Asterisk Module

In `asterisk_module` you will find a completely new module called `chan_sccp2`. 


## Developing

`git submodule update --init --recursive`

## Contributing

PRs welcome

## License

MIT
