use std::fs;
use std::net::{Ipv4Addr, SocketAddr};
use std::ops::RangeInclusive;
use std::path::Path;

use ipnet::Ipv4Net;
use sccp_protocol::{ButtonDefinition, DeviceDefinition, DeviceId, LineAppearance, LineDefinition};
use serde::Deserialize;
use thiserror::Error;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub sccp: SccpConfig,
    pub sip: SipConfig,
    pub media: MediaConfig,
    pub phones: Vec<PhoneConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SccpConfig {
    #[serde(default = "default_sccp_bind")]
    pub bind: SocketAddr,
    #[serde(default = "default_keepalive")]
    pub keepalive_seconds: u32,
    #[serde(default = "default_server_name")]
    pub server_name: String,
    #[serde(default)]
    pub firmware_version: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SipConfig {
    #[serde(default = "default_sip_bind")]
    pub bind: SocketAddr,
    #[serde(default)]
    pub advertised_address: Option<Ipv4Addr>,
    #[serde(default)]
    pub conference_feature_code: Option<String>,
    #[serde(default = "default_interdigit_ms")]
    pub interdigit_timeout_ms: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaConfig {
    pub bind_address: Ipv4Addr,
    pub advertised_address: Ipv4Addr,
    #[serde(deserialize_with = "deserialize_port_range")]
    pub port_range: RangeInclusive<u16>,
    /// Explicit network paths over which a handset and SIP peer can exchange
    /// RTP directly. An empty list keeps all calls on the relay.
    #[serde(default)]
    pub direct_routes: Vec<DirectMediaRouteConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectMediaRouteConfig {
    pub phones: Vec<Ipv4Net>,
    pub sip: Vec<Ipv4Net>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhoneConfig {
    pub device: String,
    #[serde(default)]
    pub description: String,
    pub lines: Vec<LineConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LineConfig {
    pub number: String,
    #[serde(default)]
    pub display_name: String,
    pub registrar: String,
    #[serde(default)]
    pub outbound_proxy: Option<String>,
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub auth_username: Option<String>,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("unable to read configuration {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("invalid TOML configuration: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("invalid configuration: {0}")]
    Invalid(String),
    #[error(transparent)]
    Sccp(#[from] sccp_protocol::CodecError),
}

impl AppConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let contents = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::parse(&contents)
    }

    pub fn parse(contents: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(contents)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if !self.sccp.bind.is_ipv4() {
            return Err(ConfigError::Invalid(
                "sccp.bind must be an IPv4 socket address".into(),
            ));
        }
        if !self.sip.bind.is_ipv4() {
            return Err(ConfigError::Invalid(
                "sip.bind must be an IPv4 socket address".into(),
            ));
        }
        if self.media.advertised_address.is_unspecified() {
            return Err(ConfigError::Invalid(
                "media.advertised_address must be an address reachable by phones and SIP peers"
                    .into(),
            ));
        }
        if self
            .sip
            .advertised_address
            .is_some_and(|ip| ip.is_unspecified())
        {
            return Err(ConfigError::Invalid(
                "sip.advertised_address cannot be 0.0.0.0".into(),
            ));
        }
        if self.phones.is_empty() {
            return Err(ConfigError::Invalid(
                "at least one phone is required".into(),
            ));
        }
        if self.sccp.keepalive_seconds < 5 {
            return Err(ConfigError::Invalid(
                "sccp.keepalive_seconds must be at least 5".into(),
            ));
        }
        if self.sip.interdigit_timeout_ms < 250 {
            return Err(ConfigError::Invalid(
                "sip.interdigit_timeout_ms must be at least 250".into(),
            ));
        }
        if !self.media.port_range.start().is_multiple_of(2)
            || self.media.port_range.end() - self.media.port_range.start() < 7
        {
            return Err(ConfigError::Invalid(
                "media.port_range must begin on an even port and contain at least eight ports"
                    .into(),
            ));
        }
        for (index, route) in self.media.direct_routes.iter().enumerate() {
            if route.phones.is_empty() || route.sip.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "media.direct_routes[{index}] must contain at least one phone CIDR and one SIP CIDR"
                )));
            }
        }
        let mut ids = std::collections::HashSet::new();
        for phone in &self.phones {
            let id = DeviceId::new(&phone.device)?;
            if !ids.insert(id.clone()) {
                return Err(ConfigError::Invalid(format!("duplicate phone {id}")));
            }
            if phone.lines.is_empty() || phone.lines.len() > 6 {
                return Err(ConfigError::Invalid(format!(
                    "phone {id} must define between one and six lines"
                )));
            }
            for line in &phone.lines {
                if !line.registrar.starts_with("sip:") && !line.registrar.starts_with("sips:") {
                    return Err(ConfigError::Invalid(format!(
                        "line {} registrar must be a SIP URI",
                        line.number
                    )));
                }
                if line.username.is_empty() || line.password.is_empty() {
                    return Err(ConfigError::Invalid(format!(
                        "line {} requires username and password",
                        line.number
                    )));
                }
            }
        }
        Ok(())
    }

    pub fn sccp_definitions(&self) -> Result<Vec<DeviceDefinition>, ConfigError> {
        self.phones
            .iter()
            .map(|phone| {
                let id = DeviceId::new(&phone.device)?;
                let buttons = phone
                    .lines
                    .iter()
                    .enumerate()
                    .map(|(index, line)| {
                        ButtonDefinition::Line(LineAppearance::new(
                            index as u32 + 1,
                            LineDefinition {
                                number: line.number.clone(),
                                display_name: if line.display_name.is_empty() {
                                    line.number.clone()
                                } else {
                                    line.display_name.clone()
                                },
                            },
                        ))
                    })
                    .collect();
                Ok(DeviceDefinition {
                    id,
                    description: if phone.description.is_empty() {
                        phone.device.clone()
                    } else {
                        phone.description.clone()
                    },
                    transport: sccp_protocol::StationTransportRequirement::Either,
                    signaling_qos: None,
                    buttons,
                    soft_keys: Default::default(),
                    ui: Default::default(),
                })
            })
            .collect()
    }

    pub fn phone(&self, id: &DeviceId) -> Option<&PhoneConfig> {
        self.phones
            .iter()
            .find(|phone| phone.device.eq_ignore_ascii_case(id.as_str()))
    }
}

fn deserialize_port_range<'de, D>(deserializer: D) -> Result<RangeInclusive<u16>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    let (start, end) = value
        .split_once('-')
        .ok_or_else(|| serde::de::Error::custom("expected START-END"))?;
    let start: u16 = start.parse().map_err(serde::de::Error::custom)?;
    let end: u16 = end.parse().map_err(serde::de::Error::custom)?;
    if start > end {
        return Err(serde::de::Error::custom("port range start exceeds end"));
    }
    Ok(start..=end)
}

fn default_sccp_bind() -> SocketAddr {
    "0.0.0.0:2000".parse().unwrap()
}
fn default_sip_bind() -> SocketAddr {
    "0.0.0.0:5060".parse().unwrap()
}
fn default_keepalive() -> u32 {
    30
}
fn default_server_name() -> String {
    "sccp-protocol".into()
}
fn default_interdigit_ms() -> u64 {
    3000
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
        [sccp]
        bind = "127.0.0.1:2000"

        [sip]
        bind = "127.0.0.1:5060"
        conference_feature_code = "*3"

        [media]
        bind_address = "127.0.0.1"
        advertised_address = "10.0.0.5"
        port_range = "30000-30099"

        [[media.direct_routes]]
        phones = ["10.0.0.0/8"]
        sip = ["172.16.0.0/12"]

        [[phones]]
        device = "sep001122334455"

        [[phones.lines]]
        number = "1001"
        display_name = "Desk 1001"
        registrar = "sip:asterisk.test"
        username = "1001"
        password = "secret"
    "#;

    #[test]
    fn parses_minimal_phone_and_sip_pair() {
        let config = AppConfig::parse(CONFIG).unwrap();
        let definitions = config.sccp_definitions().unwrap();
        assert_eq!(definitions[0].id.as_str(), "SEP001122334455");
        assert_eq!(definitions[0].first_line().unwrap().instance, 1);
        assert_eq!(config.media.port_range, 30000..=30099);
        assert_eq!(config.media.direct_routes.len(), 1);
    }

    #[test]
    fn example_configuration_stays_valid() {
        AppConfig::parse(include_str!("../config.example.toml")).unwrap();
    }

    #[test]
    fn rejects_duplicate_devices() {
        let duplicate = format!(
            "{CONFIG}\n[[phones]]\ndevice='SEP001122334455'\n[[phones.lines]]\nnumber='2'\nregistrar='sip:x'\nusername='x'\npassword='x'"
        );
        assert!(matches!(
            AppConfig::parse(&duplicate),
            Err(ConfigError::Invalid(_))
        ));
    }

    #[test]
    fn rejects_an_unspecified_advertised_media_address() {
        let invalid = CONFIG.replace(
            "advertised_address = \"10.0.0.5\"",
            "advertised_address = \"0.0.0.0\"",
        );
        assert!(matches!(
            AppConfig::parse(&invalid),
            Err(ConfigError::Invalid(_))
        ));
    }

    #[test]
    fn parses_multiple_lines_as_members_of_one_phone() {
        let config = format!(
            "{CONFIG}\n[[phones.lines]]\nnumber='1002'\nregistrar='sip:asterisk.test'\nusername='1002'\npassword='secret'"
        );
        let config = AppConfig::parse(&config).unwrap();
        let definitions = config.sccp_definitions().unwrap();
        assert_eq!(config.phones.len(), 1);
        assert_eq!(config.phones[0].lines.len(), 2);
        assert_eq!(definitions[0].lines().nth(1).unwrap().instance, 2);
    }

    #[test]
    fn rejects_direct_route_with_an_empty_side() {
        let invalid = CONFIG.replace("phones = [\"10.0.0.0/8\"]", "phones = []");
        assert!(matches!(
            AppConfig::parse(&invalid),
            Err(ConfigError::Invalid(_))
        ));
    }

    #[test]
    fn rejects_removed_direct_media_realms_field() {
        let invalid = CONFIG.replace(
            "[[media.direct_routes]]\n        phones = [\"10.0.0.0/8\"]\n        sip = [\"172.16.0.0/12\"]",
            "direct_media_realms = [\"10.0.0.0/8\"]",
        );
        assert!(matches!(
            AppConfig::parse(&invalid),
            Err(ConfigError::Toml(_))
        ));
    }
}
