//! Menu phone XML document family.

use super::*;

/// A directory entry containing an optional display name and dialable value.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CiscoIpPhoneDirectoryEntry {
    #[serde(rename = "Name", default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "Telephone", default, skip_serializing_if = "Option::is_none")]
    pub telephone: Option<String>,
}

/// A complete, schema-ordered phone directory response.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename = "CiscoIPPhoneDirectory", deny_unknown_fields)]
pub struct CiscoIpPhoneDirectory {
    #[serde(
        rename = "@keypadTarget",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub keypad_target: Option<PhoneKeypadTarget>,
    #[serde(rename = "@appId", default, skip_serializing_if = "Option::is_none")]
    pub application_id: Option<String>,
    #[serde(
        rename = "@onAppFocusLost",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_lost: Option<String>,
    #[serde(
        rename = "@onAppFocusGained",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_gained: Option<String>,
    #[serde(
        rename = "@onAppMinimized",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_minimized: Option<String>,
    #[serde(
        rename = "@onAppClosed",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_closed: Option<String>,
    #[serde(rename = "Title", default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "Prompt", default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(rename = "SoftKeyItem", default)]
    pub soft_keys: Vec<CiscoIpPhoneSoftKeyItem>,
    #[serde(rename = "KeyItem", default)]
    pub key_items: Vec<CiscoIpPhoneKeyItem>,
    #[serde(rename = "DirectoryEntry", default)]
    pub entries: Vec<CiscoIpPhoneDirectoryEntry>,
}

impl CiscoIpPhoneDirectory {
    /// Builds and validates a directory with no optional lifecycle actions.
    pub fn new(
        title: impl Into<String>,
        prompt: impl Into<String>,
        entries: Vec<CiscoIpPhoneDirectoryEntry>,
    ) -> Result<Self, PhoneXmlError> {
        let document = Self {
            keypad_target: None,
            application_id: None,
            on_focus_lost: None,
            on_focus_gained: None,
            on_minimized: None,
            on_closed: None,
            title: Some(title.into()),
            prompt: Some(prompt.into()),
            soft_keys: Vec::new(),
            key_items: Vec::new(),
            entries,
        };
        document.validate()?;
        Ok(document)
    }

    /// Checks entry counts, text bounds, lifecycle actions, and key bindings.
    pub fn validate(&self) -> Result<(), PhoneXmlError> {
        validate_count(
            "directory entries",
            self.entries.len(),
            PHONE_DIRECTORY_MAX_ENTRIES,
        )?;
        validate_count("directory soft keys", self.soft_keys.len(), 16)?;
        validate_count("directory key items", self.key_items.len(), 32)?;
        validate_optional_text("directory title", self.title.as_deref(), 0, 32)?;
        validate_optional_text("directory prompt", self.prompt.as_deref(), 0, 32)?;
        validate_optional_text(
            "directory application id",
            self.application_id.as_deref(),
            1,
            64,
        )?;
        for value in [
            self.on_focus_lost.as_deref(),
            self.on_focus_gained.as_deref(),
            self.on_minimized.as_deref(),
            self.on_closed.as_deref(),
        ] {
            validate_optional_text("directory lifecycle URL", value, 1, PHONE_XML_URL_MAX_CHARS)?;
        }
        validate_internal_action("directory onAppClosed action", self.on_closed.as_deref())?;
        for entry in &self.entries {
            validate_optional_text(
                "directory entry name",
                entry.name.as_deref(),
                0,
                PHONE_DIRECTORY_TEXT_MAX_CHARS,
            )?;
            validate_optional_text(
                "directory entry telephone",
                entry.telephone.as_deref(),
                0,
                PHONE_DIRECTORY_TEXT_MAX_CHARS,
            )?;
        }
        for soft_key in &self.soft_keys {
            validate_optional_text("directory soft-key name", soft_key.name.as_deref(), 0, 32)?;
            validate_optional_text(
                "directory soft-key URL",
                soft_key.url.as_deref(),
                0,
                PHONE_XML_URL_MAX_CHARS,
            )?;
            validate_optional_text(
                "directory soft-key down URL",
                soft_key.url_down.as_deref(),
                0,
                PHONE_XML_URL_MAX_CHARS,
            )?;
            validate_internal_action("directory soft-key URLDown", soft_key.url_down.as_deref())?;
        }
        for key_item in &self.key_items {
            validate_optional_text(
                "directory key URL",
                key_item.url.as_deref(),
                0,
                PHONE_XML_URL_MAX_CHARS,
            )?;
            validate_optional_text(
                "directory key down URL",
                key_item.url_down.as_deref(),
                0,
                PHONE_XML_URL_MAX_CHARS,
            )?;
            validate_internal_action("directory key URLDown", key_item.url_down.as_deref())?;
        }
        Ok(())
    }
}

pub(super) fn validate_optional_text(
    field: &'static str,
    value: Option<&str>,
    minimum: usize,
    maximum: usize,
) -> Result<(), PhoneXmlError> {
    let Some(value) = value else {
        return Ok(());
    };
    if !text_length_is_within(value, minimum, maximum) {
        return Err(PhoneXmlError::InvalidField {
            field,
            expected: match (minimum, maximum) {
                (0, 32) => "at most 32 characters",
                (0, 256) => "at most 256 characters",
                (1, 64) => "between 1 and 64 characters",
                (1, 256) => "between 1 and 256 characters",
                _ => "within the schema length bounds",
            },
        });
    }
    if !has_only_xml_characters(value) {
        return Err(PhoneXmlError::InvalidField {
            field,
            expected: "valid XML text without forbidden control characters",
        });
    }
    Ok(())
}

pub(super) fn has_only_xml_characters(value: &str) -> bool {
    value.chars().all(|character| {
        matches!(
            character as u32,
            0x9 | 0xA | 0xD | 0x20..=0xD7FF | 0xE000..=0xFFFD | 0x10000..=0x10FFFF
        )
    })
}

pub(super) fn action_kind(value: &str) -> PhoneActionKind {
    match value.split_once(':').map(|(scheme, _)| scheme) {
        Some(scheme)
            if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") =>
        {
            PhoneActionKind::Http
        }
        _ => PhoneActionKind::Internal,
    }
}

pub(super) fn validate_internal_action(
    field: &'static str,
    value: Option<&str>,
) -> Result<(), PhoneXmlError> {
    if value.is_some_and(|value| action_kind(value) == PhoneActionKind::Http) {
        Err(PhoneXmlError::InvalidField {
            field,
            expected: "an internal phone action, not HTTP or HTTPS",
        })
    } else {
        Ok(())
    }
}

pub(super) fn validate_displayable(
    title: Option<&str>,
    prompt: Option<&str>,
    application_id: Option<&str>,
    lifecycle_urls: [Option<&str>; 4],
    soft_keys: &[CiscoIpPhoneSoftKeyItem],
    key_items: &[CiscoIpPhoneKeyItem],
) -> Result<(), PhoneXmlError> {
    validate_optional_text("display title", title, 0, 32)?;
    validate_optional_text("display prompt", prompt, 0, 32)?;
    validate_optional_text("display application id", application_id, 1, 64)?;
    let [on_focus_lost, on_focus_gained, on_minimized, on_closed] = lifecycle_urls;
    for url in [on_focus_lost, on_focus_gained, on_minimized, on_closed] {
        validate_optional_text("display lifecycle URL", url, 1, PHONE_XML_URL_MAX_CHARS)?;
    }
    validate_internal_action("display onAppClosed action", on_closed)?;
    validate_count("display soft keys", soft_keys.len(), 16)?;
    for soft_key in soft_keys {
        validate_optional_text("display soft-key name", soft_key.name.as_deref(), 0, 32)?;
        validate_optional_text(
            "display soft-key URL",
            soft_key.url.as_deref(),
            0,
            PHONE_XML_URL_MAX_CHARS,
        )?;
        validate_optional_text(
            "display soft-key down URL",
            soft_key.url_down.as_deref(),
            0,
            PHONE_XML_URL_MAX_CHARS,
        )?;
        validate_internal_action("display soft-key URLDown", soft_key.url_down.as_deref())?;
    }
    validate_count("display key items", key_items.len(), 32)?;
    for key_item in key_items {
        validate_optional_text(
            "display key URL",
            key_item.url.as_deref(),
            0,
            PHONE_XML_URL_MAX_CHARS,
        )?;
        validate_optional_text(
            "display key down URL",
            key_item.url_down.as_deref(),
            0,
            PHONE_XML_URL_MAX_CHARS,
        )?;
        validate_internal_action("display key URLDown", key_item.url_down.as_deref())?;
    }
    Ok(())
}

pub(super) fn validate_image_display(
    title: Option<&str>,
    prompt: Option<&str>,
    application_id: Option<&str>,
    lifecycle_urls: [Option<&str>; 4],
    soft_keys: &[CiscoIpPhoneSoftKeyItem],
    key_items: &[CiscoIpPhoneKeyItem],
) -> Result<(), PhoneXmlError> {
    validate_displayable(
        title,
        prompt,
        application_id,
        lifecycle_urls,
        soft_keys,
        key_items,
    )
}

pub(super) fn validate_bitmap_image(
    location_x: Option<i16>,
    location_y: Option<i16>,
    width: u16,
    height: u16,
    depth: u16,
    data: Option<&PhoneBitmapData>,
) -> Result<(), PhoneXmlError> {
    if location_x.is_some_and(|value| !(-1..=132).contains(&value)) {
        return Err(PhoneXmlError::InvalidField {
            field: "bitmap image horizontal location",
            expected: "between -1 and 132",
        });
    }
    if location_y.is_some_and(|value| !(-1..=64).contains(&value)) {
        return Err(PhoneXmlError::InvalidField {
            field: "bitmap image vertical location",
            expected: "between -1 and 64",
        });
    }
    if !(1..=133).contains(&width) {
        return Err(PhoneXmlError::InvalidField {
            field: "bitmap image width",
            expected: "between 1 and 133",
        });
    }
    if !(1..=65).contains(&height) {
        return Err(PhoneXmlError::InvalidField {
            field: "bitmap image height",
            expected: "between 1 and 65",
        });
    }
    if !(1..=2).contains(&depth) {
        return Err(PhoneXmlError::InvalidField {
            field: "bitmap image depth",
            expected: "between 1 and 2",
        });
    }
    if let Some(data) = data {
        validate_count(
            "bitmap image data bytes",
            data.as_bytes().len(),
            PHONE_IMAGE_BITMAP_MAX_BYTES,
        )?;
    }
    Ok(())
}

pub(super) fn validate_file_image_location(
    location_x: Option<i16>,
    location_y: Option<i16>,
) -> Result<(), PhoneXmlError> {
    if location_x.is_some_and(|value| !(-1..=297).contains(&value)) {
        return Err(PhoneXmlError::InvalidField {
            field: "image-file horizontal location",
            expected: "between -1 and 297",
        });
    }
    if location_y.is_some_and(|value| !(-1..=167).contains(&value)) {
        return Err(PhoneXmlError::InvalidField {
            field: "image-file vertical location",
            expected: "between -1 and 167",
        });
    }
    Ok(())
}

pub(super) fn validate_icon_menu_items(
    items: &[CiscoIpPhoneIconMenuItem],
) -> Result<(), PhoneXmlError> {
    validate_count("icon menu items", items.len(), PHONE_ICON_MENU_MAX_ITEMS)?;
    for item in items {
        validate_optional_text("icon menu item name", item.name.as_deref(), 0, 64)?;
        validate_optional_text(
            "icon menu item URL",
            item.url.as_deref(),
            0,
            PHONE_XML_URL_MAX_CHARS,
        )?;
        if item.icon_index.is_some_and(|index| index > 9) {
            return Err(PhoneXmlError::InvalidField {
                field: "icon menu item index",
                expected: "between 0 and 9",
            });
        }
    }
    Ok(())
}

pub(super) fn validate_bitmap_icon(icon: &CiscoIpPhoneIconItem) -> Result<(), PhoneXmlError> {
    if !(1..=16).contains(&icon.width) {
        return Err(PhoneXmlError::InvalidField {
            field: "bitmap icon width",
            expected: "between 1 and 16",
        });
    }
    if !(1..=10).contains(&icon.height) {
        return Err(PhoneXmlError::InvalidField {
            field: "bitmap icon height",
            expected: "between 1 and 10",
        });
    }
    if !(1..=2).contains(&icon.depth) {
        return Err(PhoneXmlError::InvalidField {
            field: "bitmap icon depth",
            expected: "between 1 and 2",
        });
    }
    if let Some(data) = &icon.data
        && (data.len() > 80
            || data.len() % 2 != 0
            || !data.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return Err(PhoneXmlError::InvalidField {
            field: "bitmap icon data",
            expected: "at most 40 hexadecimal bytes",
        });
    }
    Ok(())
}

/// One optional label/action pair in a plain menu.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CiscoIpPhoneMenuItem {
    #[serde(rename = "Name", default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "URL", default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// A complete plain menu with optional lifecycle and physical-key actions.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename = "CiscoIPPhoneMenu", deny_unknown_fields)]
pub struct CiscoIpPhoneMenu {
    #[serde(
        rename = "@keypadTarget",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub keypad_target: Option<PhoneKeypadTarget>,
    #[serde(rename = "@appId", default, skip_serializing_if = "Option::is_none")]
    pub application_id: Option<String>,
    #[serde(
        rename = "@onAppFocusLost",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_lost: Option<String>,
    #[serde(
        rename = "@onAppFocusGained",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_gained: Option<String>,
    #[serde(
        rename = "@onAppMinimized",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_minimized: Option<String>,
    #[serde(
        rename = "@onAppClosed",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_closed: Option<String>,
    #[serde(rename = "Title", default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "Prompt", default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(rename = "SoftKeyItem", default)]
    pub soft_keys: Vec<CiscoIpPhoneSoftKeyItem>,
    #[serde(rename = "KeyItem", default)]
    pub key_items: Vec<CiscoIpPhoneKeyItem>,
    #[serde(rename = "MenuItem", default)]
    pub items: Vec<CiscoIpPhoneMenuItem>,
}

impl CiscoIpPhoneMenu {
    /// Builds and validates a menu with no optional lifecycle actions.
    pub fn new(
        title: impl Into<String>,
        prompt: impl Into<String>,
        items: Vec<CiscoIpPhoneMenuItem>,
    ) -> Result<Self, PhoneXmlError> {
        let document = Self {
            keypad_target: None,
            application_id: None,
            on_focus_lost: None,
            on_focus_gained: None,
            on_minimized: None,
            on_closed: None,
            title: Some(title.into()),
            prompt: Some(prompt.into()),
            soft_keys: Vec::new(),
            key_items: Vec::new(),
            items,
        };
        document.validate()?;
        Ok(document)
    }

    /// Validates display metadata and the bounded list of menu choices.
    pub fn validate(&self) -> Result<(), PhoneXmlError> {
        validate_displayable(
            self.title.as_deref(),
            self.prompt.as_deref(),
            self.application_id.as_deref(),
            [
                self.on_focus_lost.as_deref(),
                self.on_focus_gained.as_deref(),
                self.on_minimized.as_deref(),
                self.on_closed.as_deref(),
            ],
            &self.soft_keys,
            &self.key_items,
        )?;
        validate_count("menu items", self.items.len(), PHONE_MENU_MAX_ITEMS)?;
        for item in &self.items {
            validate_optional_text("menu item name", item.name.as_deref(), 0, 64)?;
            validate_optional_text(
                "menu item URL",
                item.url.as_deref(),
                0,
                PHONE_XML_URL_MAX_CHARS,
            )?;
        }
        Ok(())
    }
}

/// One indexed inline bitmap icon used by an icon menu.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CiscoIpPhoneIconItem {
    #[serde(rename = "Index")]
    pub index: u16,
    #[serde(rename = "Width")]
    /// Icon width in pixels, constrained to `1..=16`.
    pub width: u16,
    #[serde(rename = "Height")]
    /// Icon height in pixels, constrained to `1..=10`.
    pub height: u16,
    #[serde(rename = "Depth")]
    /// Icon bit depth, constrained to `1..=2`.
    pub depth: u16,
    #[serde(rename = "Data", default, skip_serializing_if = "Option::is_none")]
    /// Optional hexadecimal bitmap containing at most 40 bytes.
    pub data: Option<String>,
}

/// One indexed referenced icon used by an icon-file menu.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CiscoIpPhoneIconFileItem {
    #[serde(rename = "Index")]
    pub index: u16,
    #[serde(rename = "URL")]
    /// Resource URL constrained to at most 256 characters.
    pub url: String,
}

/// One optional label/action pair with an optional icon association.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CiscoIpPhoneIconMenuItem {
    #[serde(rename = "Name", default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "URL", default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(rename = "IconIndex", default, skip_serializing_if = "Option::is_none")]
    /// Icon index in `0..=9`; omission displays no icon.
    pub icon_index: Option<u16>,
}

/// Icon-bearing title used by an icon-file menu.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CiscoIpPhoneIconTitle {
    #[serde(
        rename = "@IconIndex",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    /// Icon index in `0..=9`; omission displays only title text.
    pub icon_index: Option<u16>,
    #[serde(rename = "$text", default)]
    pub text: String,
}

/// A complete menu whose icons are inline hexadecimal bitmaps.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename = "CiscoIPPhoneIconMenu", deny_unknown_fields)]
pub struct CiscoIpPhoneIconMenu {
    #[serde(
        rename = "@keypadTarget",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub keypad_target: Option<PhoneKeypadTarget>,
    #[serde(rename = "@appId", default, skip_serializing_if = "Option::is_none")]
    pub application_id: Option<String>,
    #[serde(
        rename = "@onAppFocusLost",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_lost: Option<String>,
    #[serde(
        rename = "@onAppFocusGained",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_gained: Option<String>,
    #[serde(
        rename = "@onAppMinimized",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_minimized: Option<String>,
    #[serde(
        rename = "@onAppClosed",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_closed: Option<String>,
    #[serde(rename = "Title", default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "Prompt", default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(rename = "SoftKeyItem", default)]
    pub soft_keys: Vec<CiscoIpPhoneSoftKeyItem>,
    #[serde(rename = "KeyItem", default)]
    pub key_items: Vec<CiscoIpPhoneKeyItem>,
    #[serde(rename = "MenuItem", default)]
    pub items: Vec<CiscoIpPhoneIconMenuItem>,
    #[serde(rename = "IconItem", default)]
    pub icons: Vec<CiscoIpPhoneIconItem>,
}

impl CiscoIpPhoneIconMenu {
    /// Builds and validates an inline-icon menu without lifecycle actions.
    pub fn new(
        title: impl Into<String>,
        prompt: impl Into<String>,
        items: Vec<CiscoIpPhoneIconMenuItem>,
        icons: Vec<CiscoIpPhoneIconItem>,
    ) -> Result<Self, PhoneXmlError> {
        let document = Self {
            keypad_target: None,
            application_id: None,
            on_focus_lost: None,
            on_focus_gained: None,
            on_minimized: None,
            on_closed: None,
            title: Some(title.into()),
            prompt: Some(prompt.into()),
            soft_keys: Vec::new(),
            key_items: Vec::new(),
            items,
            icons,
        };
        document.validate()?;
        Ok(document)
    }

    /// Validates display metadata, choice bounds, and bitmap icon geometry.
    pub fn validate(&self) -> Result<(), PhoneXmlError> {
        validate_displayable(
            self.title.as_deref(),
            self.prompt.as_deref(),
            self.application_id.as_deref(),
            [
                self.on_focus_lost.as_deref(),
                self.on_focus_gained.as_deref(),
                self.on_minimized.as_deref(),
                self.on_closed.as_deref(),
            ],
            &self.soft_keys,
            &self.key_items,
        )?;
        validate_icon_menu_items(&self.items)?;
        validate_count(
            "icon menu icons",
            self.icons.len(),
            PHONE_ICON_MENU_MAX_ICONS,
        )?;
        for icon in &self.icons {
            validate_bitmap_icon(icon)?;
        }
        Ok(())
    }
}

/// A complete menu whose icons are loaded from resource URLs.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename = "CiscoIPPhoneIconFileMenu", deny_unknown_fields)]
pub struct CiscoIpPhoneIconFileMenu {
    #[serde(
        rename = "@keypadTarget",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub keypad_target: Option<PhoneKeypadTarget>,
    #[serde(rename = "@appId", default, skip_serializing_if = "Option::is_none")]
    pub application_id: Option<String>,
    #[serde(
        rename = "@onAppFocusLost",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_lost: Option<String>,
    #[serde(
        rename = "@onAppFocusGained",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_gained: Option<String>,
    #[serde(
        rename = "@onAppMinimized",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_minimized: Option<String>,
    #[serde(
        rename = "@onAppClosed",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_closed: Option<String>,
    #[serde(
        rename = "@IconIndex",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    /// Optional icon index in `0..=9` displayed beside the title.
    pub icon_index: Option<u16>,
    #[serde(rename = "Title", default, skip_serializing_if = "Option::is_none")]
    pub title: Option<CiscoIpPhoneIconTitle>,
    #[serde(rename = "Prompt", default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(rename = "SoftKeyItem", default)]
    pub soft_keys: Vec<CiscoIpPhoneSoftKeyItem>,
    #[serde(rename = "KeyItem", default)]
    pub key_items: Vec<CiscoIpPhoneKeyItem>,
    #[serde(rename = "MenuItem", default)]
    pub items: Vec<CiscoIpPhoneIconMenuItem>,
    #[serde(rename = "IconItem", default)]
    pub icons: Vec<CiscoIpPhoneIconFileItem>,
}

impl CiscoIpPhoneIconFileMenu {
    /// Validates display metadata, choice bounds, and referenced icons.
    pub fn validate(&self) -> Result<(), PhoneXmlError> {
        validate_displayable(
            self.title.as_ref().map(|title| title.text.as_str()),
            self.prompt.as_deref(),
            self.application_id.as_deref(),
            [
                self.on_focus_lost.as_deref(),
                self.on_focus_gained.as_deref(),
                self.on_minimized.as_deref(),
                self.on_closed.as_deref(),
            ],
            &self.soft_keys,
            &self.key_items,
        )?;
        validate_icon_menu_items(&self.items)?;
        validate_count(
            "icon-file menu icons",
            self.icons.len(),
            PHONE_ICON_MENU_MAX_ICONS,
        )?;
        for icon in &self.icons {
            if icon.index > 9 {
                return Err(PhoneXmlError::InvalidField {
                    field: "icon-file index",
                    expected: "between 0 and 9",
                });
            }
            validate_optional_text("icon-file URL", Some(&icon.url), 1, PHONE_XML_URL_MAX_CHARS)?;
        }
        Ok(())
    }
}
