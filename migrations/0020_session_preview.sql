-- The last thing the user said in each session, so a list can tell apart
-- sessions that share a default name. Kept raw (bounded); cleaned when read.
ALTER TABLE sessions ADD COLUMN preview TEXT;
UPDATE sessions SET preview = (
    SELECT substr(json_extract(e.event_json, '$.text'), 1, 400)
    FROM conversation_events AS e
    WHERE e.session_id = sessions.id
      AND json_extract(e.event_json, '$.type') = 'message'
      AND json_extract(e.event_json, '$.role') = 'user'
    ORDER BY e.seq DESC LIMIT 1
);
