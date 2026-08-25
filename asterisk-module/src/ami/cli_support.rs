//! Scalar parsers shared by the inventory and diagnostic CLI surfaces.

use std::str::FromStr;

use sccp_protocol::DeviceId;

pub(super) fn parse_device<E>(value: &str, invalid: impl FnOnce() -> E) -> Result<DeviceId, E> {
    DeviceId::new(value).map_err(|_| invalid())
}

pub(super) fn parse_positive<T, E>(value: &str, invalid: impl FnOnce() -> E) -> Result<T, E>
where
    T: FromStr + Default + PartialEq,
{
    value
        .parse::<T>()
        .ok()
        .filter(|value| *value != T::default())
        .ok_or_else(invalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_and_positive_parsers_enforce_shared_bounds() {
        assert!(parse_device("SEP001122334455", || ()).is_ok());
        assert!(parse_device("", || ()).is_err());
        assert_eq!(parse_positive::<u32, _>("7", || ()), Ok(7));
        assert!(parse_positive::<u32, _>("0", || ()).is_err());
        assert!(parse_positive::<u32, _>("-1", || ()).is_err());
    }
}
