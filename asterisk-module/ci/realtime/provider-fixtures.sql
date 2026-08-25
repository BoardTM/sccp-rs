INSERT INTO sccp2_realtime_generations (id) VALUES (1), (3), (4);

INSERT INTO sccp2_realtime_sections
    (generation_id, family, name, section_position)
VALUES
    (1, 'device', 'SEP000000000001', 0),
    (1, 'line', '1000', 0),
    (3, 'device', 'SEP000000000003', 0),
    (3, 'line', '3000', 0),
    (4, 'device', 'SEP000000000004', 0),
    (4, 'line', '4000', 0);

INSERT INTO sccp2_realtime_fields
    (generation_id, family, section_name, field_position, field_name, field_value)
VALUES
    (1, 'device', 'SEP000000000001', 0, 'description', 'First value'),
    (1, 'device', 'SEP000000000001', 1, 'button', 'line,1000'),
    (1, 'device', 'SEP000000000001', 2, 'button', 'speed_dial,Support,2000'),
    (1, 'device', 'SEP000000000001', 3, 'description', 'Ordered value'),
    (1, 'line', '1000', 0, 'label', 'Reception'),
    (1, 'line', '1000', 1, 'context', 'from-database'),
    (3, 'device', 'SEP000000000003', 0, 'description', 'Desk å'),
    (3, 'device', 'SEP000000000003', 1, 'button', 'line,3000'),
    (3, 'device', 'SEP000000000003', 2, 'button', 'speed_dial,Operations,3001'),
    (3, 'line', '3000', 0, 'label', 'Complete å'),
    (3, 'line', '3000', 1, 'context', 'from-database'),
    (4, 'device', 'SEP000000000004', 0, 'description', 'Rejected device'),
    (4, 'device', 'SEP000000000004', 1, 'button', 'line,4000'),
    (4, 'device', 'SEP000000000004', 2, 'unknown_setting', 'invalid'),
    (4, 'line', '4000', 0, 'label', 'Rejected line'),
    (4, 'line', '4000', 1, 'context', 'from-database');

INSERT INTO sccp2_realtime_active_generation (singleton, generation_id)
VALUES (1, 1);
