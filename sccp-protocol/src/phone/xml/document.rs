//! Shared workflow for complete, schema-checked phone XML documents.
//!
//! Application code can use [`PhoneXmlDocument::parse_xml`] and
//! [`PhoneXmlDocument::serialize_xml`] uniformly across supported display
//! roots. The `*_with_limit` methods add a stricter per-request resource limit
//! without weakening a document type's validation rules.

use quick_xml::events::Event;
use quick_xml::reader::Reader;
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::{
    CiscoIpPhoneDirectory, CiscoIpPhoneExecute, CiscoIpPhoneIconFileMenu, CiscoIpPhoneIconMenu,
    CiscoIpPhoneImageList, CiscoIpPhoneInput, CiscoIpPhoneMenu, CiscoIpPhoneStatus,
    CiscoIpPhoneStatusFile, CiscoIpPhoneText, PHONE_BACKGROUND_LIST_MAX_BYTES,
    PHONE_DIRECTORY_MAX_BYTES, PHONE_EXECUTE_MAX_BYTES, PHONE_INPUT_MAX_BYTES,
    PHONE_MENU_MAX_BYTES, PHONE_STATUS_MAX_BYTES, PHONE_TEXT_MAX_BYTES, PhoneXmlError,
    decoding_reader, from_bytes, to_string,
};

mod sealed {
    pub trait Sealed {}
}

/// Common bounded parsing and serialization contract for a complete phone XML
/// document.
///
/// The trait is sealed because every supported root has its own schema and
/// validation policy. It centralizes the security boundary without allowing a
/// downstream type to opt into parsing with a guessed root or size limit.
pub trait PhoneXmlDocument: sealed::Sealed + Sized + Serialize + DeserializeOwned {
    /// Exact root element accepted for this document model.
    const ROOT: &'static [u8];
    /// Default maximum encoded document size, in bytes.
    const MAXIMUM_BYTES: usize;

    /// Checks schema invariants that cannot be expressed by Serde types alone.
    fn validate_document(&self) -> Result<(), PhoneXmlError>;

    /// Parses the exact schema root within [`Self::MAXIMUM_BYTES`].
    fn parse_xml(document: &[u8]) -> Result<Self, PhoneXmlError> {
        Self::parse_xml_with_limit(document, Self::MAXIMUM_BYTES)
    }

    /// Parses the exact schema root within a caller-selected byte limit.
    ///
    /// This is useful when a transport or device profile imposes a bound
    /// smaller than [`Self::MAXIMUM_BYTES`].
    fn parse_xml_with_limit(document: &[u8], maximum_bytes: usize) -> Result<Self, PhoneXmlError> {
        let parsed: Self = from_bytes(document, maximum_bytes)?;
        validate_root(document, Self::ROOT)?;
        parsed.validate_document()?;
        Ok(parsed)
    }

    /// Validates and serializes the document within [`Self::MAXIMUM_BYTES`].
    fn serialize_xml(&self) -> Result<String, PhoneXmlError> {
        self.serialize_xml_with_limit(Self::MAXIMUM_BYTES)
    }

    /// Validates and serializes the document within a caller-selected byte limit.
    fn serialize_xml_with_limit(&self, maximum_bytes: usize) -> Result<String, PhoneXmlError> {
        self.validate_document()?;
        to_string(self, maximum_bytes)
    }
}

macro_rules! impl_phone_xml_document {
    ($document:ty, $root:literal, $maximum:expr) => {
        impl sealed::Sealed for $document {}

        impl PhoneXmlDocument for $document {
            const ROOT: &'static [u8] = $root;
            const MAXIMUM_BYTES: usize = $maximum;

            fn validate_document(&self) -> Result<(), PhoneXmlError> {
                <$document>::validate(self)
            }
        }

        impl $document {
            /// Parses and validates this document using its schema byte limit.
            pub fn from_xml(document: &[u8]) -> Result<Self, PhoneXmlError> {
                <Self as PhoneXmlDocument>::parse_xml(document)
            }

            /// Parses and validates this document with a stricter byte limit.
            pub fn from_xml_with_limit(
                document: &[u8],
                maximum_bytes: usize,
            ) -> Result<Self, PhoneXmlError> {
                <Self as PhoneXmlDocument>::parse_xml_with_limit(document, maximum_bytes)
            }

            /// Validates and serializes this document using its schema byte limit.
            pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
                <Self as PhoneXmlDocument>::serialize_xml(self)
            }

            /// Validates and serializes this document with a stricter byte limit.
            pub fn to_xml_with_limit(&self, maximum_bytes: usize) -> Result<String, PhoneXmlError> {
                <Self as PhoneXmlDocument>::serialize_xml_with_limit(self, maximum_bytes)
            }
        }
    };
}

impl_phone_xml_document!(CiscoIpPhoneText, b"CiscoIPPhoneText", PHONE_TEXT_MAX_BYTES);
impl_phone_xml_document!(
    CiscoIpPhoneInput,
    b"CiscoIPPhoneInput",
    PHONE_INPUT_MAX_BYTES
);
impl_phone_xml_document!(
    CiscoIpPhoneExecute,
    b"CiscoIPPhoneExecute",
    PHONE_EXECUTE_MAX_BYTES
);
impl_phone_xml_document!(
    CiscoIpPhoneImageList,
    b"CiscoIPPhoneImageList",
    PHONE_BACKGROUND_LIST_MAX_BYTES
);
impl_phone_xml_document!(
    CiscoIpPhoneStatus,
    b"CiscoIPPhoneStatus",
    PHONE_STATUS_MAX_BYTES
);
impl_phone_xml_document!(
    CiscoIpPhoneStatusFile,
    b"CiscoIPPhoneStatusFile",
    PHONE_STATUS_MAX_BYTES
);
impl_phone_xml_document!(
    CiscoIpPhoneDirectory,
    b"CiscoIPPhoneDirectory",
    PHONE_DIRECTORY_MAX_BYTES
);
impl_phone_xml_document!(CiscoIpPhoneMenu, b"CiscoIPPhoneMenu", PHONE_MENU_MAX_BYTES);
impl_phone_xml_document!(
    CiscoIpPhoneIconMenu,
    b"CiscoIPPhoneIconMenu",
    PHONE_MENU_MAX_BYTES
);
impl_phone_xml_document!(
    CiscoIpPhoneIconFileMenu,
    b"CiscoIPPhoneIconFileMenu",
    PHONE_MENU_MAX_BYTES
);

fn validate_root(document: &[u8], expected: &[u8]) -> Result<(), PhoneXmlError> {
    let mut reader = Reader::from_reader(decoding_reader(document));
    let mut buffer = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(element) | Event::Empty(element)) => {
                return if element.name().as_ref().as_bytes() == expected {
                    Ok(())
                } else {
                    Err(PhoneXmlError::InvalidField {
                        field: "phone XML document root",
                        expected: "the schema root for this document type",
                    })
                };
            }
            Ok(Event::Eof) => {
                return Err(PhoneXmlError::InvalidField {
                    field: "phone XML document root",
                    expected: "one schema root element",
                });
            }
            Ok(_) => {}
            Err(error) => return Err(PhoneXmlError::Malformed(error)),
        }
        buffer.clear();
    }
}
