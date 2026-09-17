-- Native message/part storage used by OpenCode v1.18.31's CLI.
-- Only columns needed by these fixtures are included.
CREATE TABLE session (
    id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT, title TEXT,
    time_created INTEGER, time_updated INTEGER, time_archived INTEGER, cost REAL
);
CREATE TABLE message (
    id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT
);
CREATE TABLE part (
    id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, data TEXT
);
INSERT INTO session VALUES
    ('ses_fixture', NULL, '/fixture', 'Named OpenCode', 1789552800000, 1789552803000, NULL, 0.3),
    ('ses_second', NULL, '/second', 'Another conversation', 1789552700000, 1789552701000, NULL, 0),
    ('ses_archived', NULL, '/fixture', 'Archived conversation', 1789552600000, 1789552601000, 1789552800000, 0),
    ('ses_child', 'ses_fixture', '/fixture', 'Subagent', 1789552800000, 1789552803000, NULL, 0),
    ('ses_empty', NULL, '', 'Missing directory', 1789552800000, 1789552803000, NULL, 0);
INSERT INTO message VALUES
    ('msg_user', 'ses_fixture', 1789552801000, '{"role":"user","time":{"created":1789552801000}}'),
    ('msg_a', 'ses_fixture', 1789552802000, '{"role":"assistant","modelID":"fixture-model","providerID":"fixture-provider","tokens":{"input":3,"output":2,"reasoning":1,"cache":{"read":4,"write":5}},"cost":0.1,"finish":"tool-calls","time":{"created":1789552802000,"completed":1789552802100}}'),
    ('msg_b', 'ses_fixture', 1789552803000, '{"role":"assistant","modelID":"fixture-model","providerID":"fixture-provider","tokens":{"input":20,"output":3,"reasoning":2,"cache":{"read":10,"write":1}},"cost":0.2,"finish":"stop","time":{"created":1789552803000,"completed":1789552803100}}'),
    ('msg_other', 'ses_second', 1789552701000, '{"role":"assistant","modelID":"other-model","providerID":"other-provider","tokens":{"input":1,"output":1,"reasoning":0,"cache":{"read":0,"write":0}},"cost":0}');
INSERT INTO part VALUES
    ('prt_u1', 'msg_user', 'ses_fixture', '{"type":"text","text":"Inspect the fixture\nand explain it"}'),
    ('prt_u2', 'msg_user', 'ses_fixture', '{"type":"text","text":"injected instructions","synthetic":true}'),
    ('prt_u3', 'msg_user', 'ses_fixture', '{"type":"text","text":"ignored text","ignored":true}'),
    ('prt_a1', 'msg_a', 'ses_fixture', '{"type":"text","text":"Reading"}'),
    ('prt_a2', 'msg_a', 'ses_fixture', '{"type":"tool","tool":"read","state":{"status":"completed","output":"tool payload"}}'),
    ('prt_b1', 'msg_b', 'ses_fixture', '{"type":"reasoning","text":"private reasoning"}'),
    ('prt_b2', 'msg_b', 'ses_fixture', '{"type":"text","text":"Done\nAdditional detail"}'),
    ('prt_other', 'msg_other', 'ses_second', '{"type":"text","text":"Other answer"}');
