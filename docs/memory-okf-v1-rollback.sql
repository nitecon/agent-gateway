-- Offline, opt-in destructive rollback of ONLY optional OKF metadata.
-- Back up the database first. Legacy binaries can also leave these columns intact.
BEGIN IMMEDIATE;
ALTER TABLE gateway_memories DROP COLUMN okf_json;
ALTER TABLE gateway_memories DROP COLUMN concept_hash;
COMMIT;
