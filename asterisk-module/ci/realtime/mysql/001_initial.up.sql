CREATE TABLE sccp2_realtime_generations (
    id BIGINT UNSIGNED NOT NULL PRIMARY KEY CHECK (id > 0),
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
) ENGINE = InnoDB;

CREATE TABLE sccp2_realtime_active_generation (
    singleton BOOLEAN NOT NULL PRIMARY KEY DEFAULT TRUE CHECK (singleton = TRUE),
    generation_id BIGINT UNSIGNED NOT NULL UNIQUE,
    FOREIGN KEY (generation_id) REFERENCES sccp2_realtime_generations (id)
) ENGINE = InnoDB;

CREATE TABLE sccp2_realtime_sections (
    generation_id BIGINT UNSIGNED NOT NULL,
    family VARCHAR(16) NOT NULL CHECK (family IN ('device', 'line')),
    name VARCHAR(255) NOT NULL CHECK (length(trim(name)) > 0),
    section_position BIGINT UNSIGNED NOT NULL CHECK (
        section_position <= 9223372036853
    ),
    PRIMARY KEY (generation_id, family, name),
    UNIQUE (generation_id, family, section_position),
    FOREIGN KEY (generation_id) REFERENCES sccp2_realtime_generations (id)
        ON DELETE CASCADE
) ENGINE = InnoDB;

CREATE TABLE sccp2_realtime_fields (
    generation_id BIGINT UNSIGNED NOT NULL,
    family VARCHAR(16) NOT NULL,
    section_name VARCHAR(255) NOT NULL,
    field_position INT UNSIGNED NOT NULL CHECK (field_position < 1000000),
    field_name VARCHAR(255) NOT NULL CHECK (
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
) ENGINE = InnoDB;

CREATE VIEW sccp_devices AS
SELECT
    CAST(0 AS UNSIGNED) AS _row_order,
    CAST(active.generation_id AS CHAR(20)) AS _revision,
    1 AS _metadata,
    CAST(NULL AS CHAR(255)) AS name,
    CAST(NULL AS CHAR(255)) AS _field_name,
    CAST(NULL AS CHAR(8)) AS _field_kind,
    CAST(NULL AS CHAR) AS _field_value
FROM sccp2_realtime_active_generation AS active
UNION ALL
SELECT
    section.section_position * 1000000 + field.field_position + 1,
    CAST(section.generation_id AS CHAR(20)),
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
        ELSE lower(hex(field.field_value))
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
    CAST(0 AS UNSIGNED) AS _row_order,
    CAST(active.generation_id AS CHAR(20)) AS _revision,
    1 AS _metadata,
    CAST(NULL AS CHAR(255)) AS name,
    CAST(NULL AS CHAR(255)) AS _field_name,
    CAST(NULL AS CHAR(8)) AS _field_kind,
    CAST(NULL AS CHAR) AS _field_value
FROM sccp2_realtime_active_generation AS active
UNION ALL
SELECT
    section.section_position * 1000000 + field.field_position + 1,
    CAST(section.generation_id AS CHAR(20)),
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
        ELSE lower(hex(field.field_value))
    END
FROM sccp2_realtime_sections AS section
JOIN sccp2_realtime_fields AS field
  ON field.generation_id = section.generation_id
 AND field.family = section.family
 AND field.section_name = section.name
JOIN sccp2_realtime_active_generation AS active
  ON active.generation_id = section.generation_id
WHERE section.family = 'line';
