-- retired_legacy_doc_ids contains the legacy doc ids supplied by the caller.
-- The caller runs this in a transaction with deferred foreign keys.
CREATE TEMP TABLE retired_documentation(id TEXT PRIMARY KEY);
INSERT INTO retired_documentation
    SELECT artifact_id FROM artifacts WHERE kind = 'documentation';
CREATE TEMP TABLE retired_documentation_entities(id TEXT PRIMARY KEY);
INSERT INTO retired_documentation_entities SELECT id FROM retired_documentation;
INSERT OR IGNORE INTO retired_documentation_entities SELECT id FROM retired_legacy_doc_ids;
INSERT OR IGNORE INTO retired_documentation_entities
    SELECT artifact_version_id FROM artifact_versions
    WHERE artifact_id IN (SELECT id FROM retired_documentation);
INSERT OR IGNORE INTO retired_documentation_entities
    SELECT contribution_id FROM artifact_contributions
    WHERE artifact_id IN (SELECT id FROM retired_documentation);
INSERT OR IGNORE INTO retired_documentation_entities
    SELECT comment_id FROM artifact_comments
    WHERE artifact_id IN (SELECT id FROM retired_documentation);
INSERT OR IGNORE INTO retired_documentation_entities
    SELECT chunk_id FROM artifact_chunks
    WHERE artifact_id IN (SELECT id FROM retired_documentation);
CREATE TEMP TABLE retired_documentation_runs(id TEXT PRIMARY KEY);
INSERT INTO retired_documentation_runs
    SELECT workflow_run_id FROM workflow_runs
    WHERE artifact_id IN (SELECT id FROM retired_documentation)
       OR workflow_kind = 'doc_publish';
CREATE TEMP TABLE retired_documentation_links(id TEXT PRIMARY KEY);
INSERT INTO retired_documentation_links
    WITH RECURSIVE doomed(id) AS (
        SELECT link_id FROM artifact_links
        WHERE source_id IN (SELECT id FROM retired_documentation_entities)
           OR target_id IN (SELECT id FROM retired_documentation_entities)
           OR source_version_id IN (SELECT id FROM retired_documentation_entities)
           OR target_version_id IN (SELECT id FROM retired_documentation_entities)
           OR created_via_workflow_run_id IN (SELECT id FROM retired_documentation_runs)
        UNION
        SELECT l.link_id FROM artifact_links l JOIN doomed d ON l.supersedes_link_id = d.id
    ) SELECT id FROM doomed;
DELETE FROM artifact_links WHERE link_id IN (SELECT id FROM retired_documentation_links);
DELETE FROM artifact_comments WHERE artifact_id IN (SELECT id FROM retired_documentation);
DELETE FROM artifact_contributions WHERE artifact_id IN (SELECT id FROM retired_documentation);
DELETE FROM artifact_chunks WHERE artifact_id IN (SELECT id FROM retired_documentation);
DELETE FROM artifact_versions WHERE artifact_id IN (SELECT id FROM retired_documentation);
DELETE FROM workflow_runs WHERE workflow_run_id IN (SELECT id FROM retired_documentation_runs);
DELETE FROM artifacts WHERE artifact_id IN (SELECT id FROM retired_documentation);
DROP TABLE IF EXISTS api_docs;
DROP TABLE retired_documentation_links;
DROP TABLE retired_documentation_runs;
DROP TABLE retired_documentation_entities;
DROP TABLE retired_documentation;
DROP TABLE retired_legacy_doc_ids;
-- Older schemas have wider CHECK constraints. Prevent old clients from
-- recreating retired records without rebuilding shared artifact tables.
CREATE TRIGGER IF NOT EXISTS reject_documentation_artifact_insert
BEFORE INSERT ON artifacts WHEN NEW.kind = 'documentation'
BEGIN SELECT RAISE(ABORT, 'documentation feature retired'); END;
CREATE TRIGGER IF NOT EXISTS reject_documentation_artifact_update
BEFORE UPDATE OF kind ON artifacts WHEN NEW.kind = 'documentation'
BEGIN SELECT RAISE(ABORT, 'documentation feature retired'); END;
CREATE TRIGGER IF NOT EXISTS reject_doc_publish_insert
BEFORE INSERT ON workflow_runs WHEN NEW.workflow_kind = 'doc_publish'
BEGIN SELECT RAISE(ABORT, 'documentation feature retired'); END;
CREATE TRIGGER IF NOT EXISTS reject_doc_publish_update
BEFORE UPDATE OF workflow_kind ON workflow_runs WHEN NEW.workflow_kind = 'doc_publish'
BEGIN SELECT RAISE(ABORT, 'documentation feature retired'); END;
CREATE TRIGGER IF NOT EXISTS reject_documentation_version_format
BEFORE INSERT ON artifact_versions WHEN NEW.body_format != 'markdown'
BEGIN SELECT RAISE(ABORT, 'documentation formats retired'); END;
CREATE TRIGGER IF NOT EXISTS reject_documentation_contribution_format
BEFORE INSERT ON artifact_contributions WHEN NEW.body_format != 'markdown'
BEGIN SELECT RAISE(ABORT, 'documentation formats retired'); END;
