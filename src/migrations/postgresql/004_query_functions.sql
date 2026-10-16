-- Functions used by the daemon's portable query vocabulary. Values continue
-- to use PostgreSQL protocol parameters; none of these functions execute SQL.
CREATE OR REPLACE FUNCTION sashiko_strftime(format TEXT, epoch BIGINT, unit TEXT)
RETURNS TEXT LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE AS $$
BEGIN
    IF format <> '%Y-%m-%d' OR unit <> 'unixepoch' THEN
        RAISE EXCEPTION 'unsupported date format or epoch unit';
    END IF;
    RETURN sashiko_day(epoch);
END
$$;

CREATE OR REPLACE FUNCTION sashiko_datetime(epoch BIGINT, unit TEXT)
RETURNS TEXT LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE AS $$
BEGIN
    IF unit <> 'unixepoch' THEN RAISE EXCEPTION 'unsupported epoch unit'; END IF;
    RETURN sashiko_datetime(epoch);
END
$$;

CREATE OR REPLACE FUNCTION sashiko_json_ids(payload TEXT)
RETURNS TABLE(value BIGINT) LANGUAGE SQL IMMUTABLE STRICT PARALLEL SAFE AS $$
    SELECT value::bigint FROM jsonb_array_elements_text(payload::jsonb) AS element(value)
$$;

CREATE OR REPLACE FUNCTION sashiko_json_set(payload TEXT, path TEXT, value TEXT)
RETURNS TEXT LANGUAGE SQL IMMUTABLE PARALLEL SAFE AS $$
    SELECT jsonb_set(payload::jsonb, string_to_array(substring(path FROM 3), '.'),
        coalesce(to_jsonb(value), 'null'::jsonb), true)::text
$$;

CREATE OR REPLACE FUNCTION sashiko_json_flag(payload TEXT, path TEXT)
RETURNS BIGINT LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE AS $$
DECLARE scalar TEXT;
BEGIN
    scalar := payload::jsonb #>> string_to_array(substring(path FROM 3), '.');
    IF scalar = 'true' THEN RETURN 1; END IF;
    IF scalar = 'false' THEN RETURN 0; END IF;
    RETURN scalar::bigint;
END
$$;
