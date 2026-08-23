PRAGMA foreign_keys = ON;

CREATE TABLE sccp2_realtime_generations (
    id INTEGER PRIMARY KEY CHECK (id > 0),
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE sccp2_realtime_active_generation (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    generation_id INTEGER NOT NULL UNIQUE,
    FOREIGN KEY (generation_id) REFERENCES sccp2_realtime_generations (id)
);

CREATE TABLE sccp2_realtime_sections (
    generation_id INTEGER NOT NULL,
    family TEXT NOT NULL CHECK (family IN ('device', 'line')),
    name TEXT NOT NULL CHECK (length(trim(name)) > 0),
    section_position INTEGER NOT NULL CHECK (
        section_position >= 0 AND section_position <= 9223372036853
    ),
    PRIMARY KEY (generation_id, family, name),
    UNIQUE (generation_id, family, section_position),
    FOREIGN KEY (generation_id) REFERENCES sccp2_realtime_generations (id)
        ON DELETE CASCADE
);

CREATE TABLE sccp2_realtime_fields (
    generation_id INTEGER NOT NULL,
    family TEXT NOT NULL,
    section_name TEXT NOT NULL,
    field_position INTEGER NOT NULL CHECK (
        field_position >= 0 AND field_position < 1000000
    ),
    field_name TEXT NOT NULL CHECK (
        length(trim(field_name)) > 0
        AND field_name NOT IN (
            'name', '_row_order', '_revision', '_metadata',
            '_field_name', '_field_kind', '_field_value'
        )
    ),
    field_value TEXT,
    PRIMARY KEY (generation_id, family, section_name, field_position),
    FOREIGN KEY (generation_id, family, section_name)
        REFERENCES sccp2_realtime_sections (generation_id, family, name)
        ON DELETE CASCADE,
    CHECK (
        field_name <> '_delete'
        OR (
            field_value IS NOT NULL
            AND lower(trim(field_value)) IN (
                'true', 'yes', 'on', '1', 'false', 'no', 'off', '0'
            )
        )
    )
);

CREATE VIEW sccp_devices AS
SELECT
    0 AS _row_order,
    CAST(active.generation_id AS TEXT) AS _revision,
    1 AS _metadata,
    NULL AS name,
    NULL AS _field_name,
    NULL AS _field_kind,
    NULL AS _field_value
FROM sccp2_realtime_active_generation AS active
UNION ALL
SELECT
    section.section_position * 1000000 + field.field_position + 1,
    CAST(section.generation_id AS TEXT),
    0,
    section.name,
    field.field_name,
    CASE
        WHEN field.field_value IS NULL THEN 'null'
        WHEN field.field_value = '' THEN 'empty'
        ELSE 'value'
    END,
    CASE
        WHEN field.field_value IS NULL OR field.field_value = '' THEN '_'
        ELSE lower(hex(CAST(field.field_value AS BLOB)))
    END
FROM sccp2_realtime_sections AS section
JOIN sccp2_realtime_fields AS field
  ON field.generation_id = section.generation_id
 AND field.family = section.family
 AND field.section_name = section.name
JOIN sccp2_realtime_active_generation AS active
  ON active.generation_id = section.generation_id
WHERE section.family = 'device';

CREATE VIEW sccp_lines AS
SELECT
    0 AS _row_order,
    CAST(active.generation_id AS TEXT) AS _revision,
    1 AS _metadata,
    NULL AS name,
    NULL AS _field_name,
    NULL AS _field_kind,
    NULL AS _field_value
FROM sccp2_realtime_active_generation AS active
UNION ALL
SELECT
    section.section_position * 1000000 + field.field_position + 1,
    CAST(section.generation_id AS TEXT),
    0,
    section.name,
    field.field_name,
    CASE
        WHEN field.field_value IS NULL THEN 'null'
        WHEN field.field_value = '' THEN 'empty'
        ELSE 'value'
    END,
    CASE
        WHEN field.field_value IS NULL OR field.field_value = '' THEN '_'
        ELSE lower(hex(CAST(field.field_value AS BLOB)))
    END
FROM sccp2_realtime_sections AS section
JOIN sccp2_realtime_fields AS field
  ON field.generation_id = section.generation_id
 AND field.family = section.family
 AND field.section_name = section.name
JOIN sccp2_realtime_active_generation AS active
  ON active.generation_id = section.generation_id
WHERE section.family = 'line';
