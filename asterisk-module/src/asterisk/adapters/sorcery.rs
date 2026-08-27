use std::sync::Arc;

use crate::asterisk::raw::sorcery::{RawSorceryObject, SorceryRegistration};
use crate::config::sorcery::{
    OrderedSorceryObject, SorceryField, SorceryInventory, SorceryObjectSource, SorcerySourceError,
};
use crate::state::persistence::PersistentStore;

use super::AsteriskDatabase;

const LKG_FAMILY: &str = "SCCP/config";
const LKG_KEY: &str = "last-known-good";

pub struct AsteriskSorcerySource {
    registration: Arc<SorceryRegistration>,
    database: AsteriskDatabase,
}

impl AsteriskSorcerySource {
    pub fn new(registration: Arc<SorceryRegistration>) -> Self {
        Self {
            registration,
            database: AsteriskDatabase::new(),
        }
    }
}

impl SorceryObjectSource for AsteriskSorcerySource {
    fn load_desired(&self) -> Result<SorceryInventory, SorcerySourceError> {
        let snapshot = self
            .registration
            .snapshot()
            .map_err(|error| SorcerySourceError::new(error.to_string()))?;
        Ok(SorceryInventory {
            devices: snapshot.devices.into_iter().map(convert_object).collect(),
            lines: snapshot.lines.into_iter().map(convert_object).collect(),
        })
    }

    fn load_last_known_good(&self) -> Result<Option<SorceryInventory>, SorcerySourceError> {
        self.database
            .get(LKG_FAMILY, LKG_KEY)
            .map_err(|error| SorcerySourceError::new(error.to_string()))?
            .map(|value| {
                serde_json::from_str(&value)
                    .map_err(|error| SorcerySourceError::new(format!("invalid LKG JSON: {error}")))
            })
            .transpose()
    }

    fn store_last_known_good(
        &self,
        inventory: &SorceryInventory,
    ) -> Result<(), SorcerySourceError> {
        let value = serde_json::to_string(inventory)
            .map_err(|error| SorcerySourceError::new(error.to_string()))?;
        self.database
            .put(LKG_FAMILY, LKG_KEY, &value)
            .map_err(|error| SorcerySourceError::new(error.to_string()))
    }
}

fn convert_object(object: RawSorceryObject) -> OrderedSorceryObject {
    OrderedSorceryObject {
        id: object.id,
        fields: object
            .fields
            .into_iter()
            .map(|field| SorceryField {
                name: field.name,
                value: field.value,
            })
            .collect(),
    }
}
