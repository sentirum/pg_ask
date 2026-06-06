-- pg_ask 0.5.6 → 0.5.7 upgrade.
--
-- Security fix: ask._config_get() previously was a SECURITY DEFINER SQL
-- function GRANTed to PUBLIC with no key filtering, so any role could read
-- secret values (api_key / embedding_api_key) in plaintext straight out of
-- ask._config. This redefines it as a plpgsql function that refuses to
-- return secret keys to non-superusers.
--
-- Fresh 0.5.7 installs get the hardened definition from bootstrap.sql; this
-- script carries the same fix to databases upgrading from 0.5.6.
--
-- CREATE OR REPLACE keeps the existing signature/ownership, so re-running is
-- safe and dependent grants are preserved.

CREATE OR REPLACE FUNCTION ask._config_get(lookup_key text)
RETURNS text
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
STABLE
AS $$
BEGIN
    IF lookup_key IN ('api_key', 'embedding_api_key') AND NOT current_setting('is_superuser')::boolean THEN
        RAISE EXCEPTION 'permission denied to read secret config key';
    END IF;
    RETURN (SELECT value FROM ask._config WHERE key = lookup_key);
END;
$$;
