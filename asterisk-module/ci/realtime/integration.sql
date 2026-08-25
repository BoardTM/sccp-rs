INSERT INTO sccp2_realtime_generations (id) VALUES (1);
INSERT INTO sccp2_realtime_sections
    (generation_id, family, name, section_position)
VALUES
    (1, 'device', 'SEP001', 0),
    (1, 'line', '1000', 0),
    (1, 'line', '1001', 1);
INSERT INTO sccp2_realtime_fields
    (generation_id, family, section_name, field_position, field_name, field_value)
VALUES
    (1, 'device', 'SEP001', 0, 'button', 'line,1000'),
    (1, 'device', 'SEP001', 1, 'button', 'speed_dial,Support,2000'),
    (1, 'device', 'SEP001', 2, 'description', NULL),
    (1, 'device', 'SEP001', 3, 'label', ''),
    (1, 'line', '1000', 0, 'label', 'Reception'),
    (1, 'line', '1001', 0, '_delete', 'yes');
INSERT INTO sccp2_realtime_active_generation (singleton, generation_id)
VALUES (1, 1);

SELECT
    'initial', 'device', view_row._row_order, view_row._revision,
    view_row.name, view_row._field_name,
    CASE
        WHEN view_row._field_kind = 'null' THEN '<NULL>'
        WHEN view_row._field_kind = 'empty' THEN ''
        ELSE field.field_value
    END
FROM sccp_devices AS view_row
JOIN sccp2_realtime_sections AS section
  ON section.generation_id = (
      SELECT generation_id FROM sccp2_realtime_active_generation
  )
 AND section.family = 'device'
 AND section.name = view_row.name
JOIN sccp2_realtime_fields AS field
  ON field.generation_id = section.generation_id
 AND field.family = section.family
 AND field.section_name = section.name
 AND field.field_name = view_row._field_name
 AND field.field_position = view_row._row_order - section.section_position * 1000000 - 1
WHERE view_row._metadata = 0
ORDER BY view_row._row_order;
SELECT
    'initial', 'line', view_row._row_order, view_row._revision,
    view_row.name, view_row._field_name,
    CASE
        WHEN view_row._field_kind = 'null' THEN '<NULL>'
        WHEN view_row._field_kind = 'empty' THEN ''
        ELSE field.field_value
    END
FROM sccp_lines AS view_row
JOIN sccp2_realtime_sections AS section
  ON section.generation_id = (
      SELECT generation_id FROM sccp2_realtime_active_generation
  )
 AND section.family = 'line'
 AND section.name = view_row.name
JOIN sccp2_realtime_fields AS field
  ON field.generation_id = section.generation_id
 AND field.family = section.family
 AND field.section_name = section.name
 AND field.field_position = view_row._row_order - section.section_position * 1000000 - 1
WHERE view_row._metadata = 0
ORDER BY view_row._row_order;

BEGIN;
INSERT INTO sccp2_realtime_generations (id) VALUES (2);
INSERT INTO sccp2_realtime_sections
    (generation_id, family, name, section_position)
VALUES (2, 'device', 'BROKEN', 0);
INSERT INTO sccp2_realtime_fields
    (generation_id, family, section_name, field_position, field_name, field_value)
VALUES (2, 'device', 'BROKEN', 0, 'unknown_setting', 'invalid');
SELECT
    'staged',
    (SELECT min(_revision) FROM sccp_devices),
    (SELECT min(_revision) FROM sccp_lines),
    (SELECT count(*) FROM sccp2_realtime_generations WHERE id = 2);
ROLLBACK;
SELECT
    'rollback',
    (SELECT min(_revision) FROM sccp_devices),
    (SELECT min(_revision) FROM sccp_lines),
    (SELECT count(*) FROM sccp2_realtime_generations WHERE id = 2);

BEGIN;
INSERT INTO sccp2_realtime_generations (id) VALUES (3);
INSERT INTO sccp2_realtime_sections
    (generation_id, family, name, section_position)
VALUES
    (3, 'device', 'SEP003', 0),
    (3, 'line', '3000', 0);
INSERT INTO sccp2_realtime_fields
    (generation_id, family, section_name, field_position, field_name, field_value)
VALUES
    (3, 'device', 'SEP003', 0, 'button', 'line,3000'),
    (3, 'line', '3000', 0, 'label', 'Complete');
UPDATE sccp2_realtime_active_generation SET generation_id = 3 WHERE singleton = 1;
COMMIT;
SELECT
    'refresh',
    (SELECT min(_revision) FROM sccp_devices),
    (SELECT min(_revision) FROM sccp_lines);
SELECT
    'refresh', 'device', _row_order, _revision, name, _field_name, _field_value
FROM sccp_devices
WHERE _metadata = 0
ORDER BY _row_order;
SELECT
    'refresh', 'line', _row_order, _revision, name, _field_name, _field_value
FROM sccp_lines
WHERE _metadata = 0
ORDER BY _row_order;
