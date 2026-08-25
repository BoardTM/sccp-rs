CREATE TABLE sccp_lines_mixed AS
SELECT
    _row_order,
    'different-revision' AS _revision,
    _metadata,
    name,
    _field_name,
    _field_kind,
    _field_value
FROM sccp_lines;

DROP VIEW sccp_lines;
ALTER TABLE sccp_lines_mixed RENAME TO sccp_lines;
