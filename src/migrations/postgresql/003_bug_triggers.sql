CREATE OR REPLACE FUNCTION sashiko_audit_bug() RETURNS TRIGGER
LANGUAGE plpgsql AS $$
DECLARE
    field_name TEXT;
    previous JSONB;
    current_value JSONB;
    payload JSONB;
BEGIN
    IF TG_OP = 'INSERT' THEN
        INSERT INTO bug_enrichments
            (bug_id, kind, tool, model, author, created_at, content, data_json)
        VALUES (NEW.id, 'audit', NEW.audit_tool, NEW.audit_model,
            NEW.audit_author, extract(epoch from now())::bigint,
            convert_to('Bug created', 'UTF8'), '{"action":"created"}');
    ELSE
        FOREACH field_name IN ARRAY ARRAY['lifecycle_status', 'title', 'duplicate_of_id', 'assignee'] LOOP
            previous := to_jsonb(OLD) -> field_name;
            current_value := to_jsonb(NEW) -> field_name;
            IF previous IS DISTINCT FROM current_value THEN
                payload := jsonb_build_object('field', field_name, 'old', previous, 'new', current_value);
                INSERT INTO bug_enrichments
                    (bug_id, kind, tool, model, author, created_at, content, data_json)
                VALUES (NEW.id, 'audit', NEW.audit_tool, NEW.audit_model,
                    NEW.audit_author, extract(epoch from now())::bigint,
                    convert_to('Field "' || field_name || '" changed', 'UTF8'), payload::text);
            END IF;
        END LOOP;
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER trg_bugs_audit AFTER INSERT OR UPDATE ON bugs
FOR EACH ROW EXECUTE FUNCTION sashiko_audit_bug();

CREATE OR REPLACE FUNCTION sashiko_audit_bug_subsystem() RETURNS TRIGGER
LANGUAGE plpgsql AS $$
DECLARE
    parent bugs%ROWTYPE;
    payload JSONB;
BEGIN
    IF TG_OP = 'UPDATE' AND OLD.source = NEW.source THEN
        RETURN NEW;
    END IF;
    SELECT * INTO parent FROM bugs WHERE id = NEW.bug_id;
    IF TG_OP = 'INSERT' THEN
        payload := jsonb_build_object('action', 'subsystem_added', 'subsystem', NEW.subsystem);
    ELSE
        payload := jsonb_build_object('action', 'subsystem_reattributed',
            'subsystem', NEW.subsystem, 'old', OLD.source, 'new', NEW.source);
    END IF;
    INSERT INTO bug_enrichments
        (bug_id, kind, tool, model, author, created_at, content, data_json)
    VALUES (NEW.bug_id, 'audit', parent.audit_tool, parent.audit_model,
        parent.audit_author, extract(epoch from now())::bigint,
        convert_to('Subsystem "' || NEW.subsystem || '" updated', 'UTF8'), payload::text);
    RETURN NEW;
END
$$;

CREATE TRIGGER trg_bug_subsystems_audit AFTER INSERT OR UPDATE OF source ON bug_subsystems
FOR EACH ROW EXECUTE FUNCTION sashiko_audit_bug_subsystem();

CREATE OR REPLACE FUNCTION sashiko_project_bug_enrichment() RETURNS TRIGGER
LANGUAGE plpgsql AS $$
DECLARE
    payload JSONB;
BEGIN
    IF NEW.data_json IS NULL THEN
        RETURN NEW;
    END IF;
    payload := NEW.data_json::jsonb;
    IF NEW.kind = 'severity_calibration' THEN
        UPDATE bugs SET severity_int = coalesce(
            CASE lower(payload ->> 'severity')
                WHEN 'critical' THEN 4 WHEN 'high' THEN 3
                WHEN 'medium' THEN 2 WHEN 'low' THEN 1 END,
            (payload ->> 'severity_int')::bigint, 0)
        WHERE id = NEW.bug_id;
    ELSIF NEW.kind = 'verification' AND payload ->> 'verified_on_sha' IS NOT NULL THEN
        UPDATE bugs SET verified_on_sha = payload ->> 'verified_on_sha' WHERE id = NEW.bug_id;
    ELSIF NEW.kind = 'origin_discovery' AND payload ->> 'introducing_commit_sha' IS NOT NULL THEN
        UPDATE bugs SET introducing_commit_sha = payload ->> 'introducing_commit_sha' WHERE id = NEW.bug_id;
    ELSIF NEW.kind = 'fix_candidate' AND payload ->> 'commit_sha' IS NOT NULL THEN
        UPDATE bugs SET fixed_in_commit_sha = payload ->> 'commit_sha' WHERE id = NEW.bug_id;
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER trg_bug_enrichments_project AFTER INSERT ON bug_enrichments
FOR EACH ROW EXECUTE FUNCTION sashiko_project_bug_enrichment();
