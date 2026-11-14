-- Example server-side helpers matching the `<ivhex>^|^<datahex>` format that db decrypts (SPEC R37)
-- and produces (SPEC R52). AES-CBC via pgcrypto; the key is the UTF-8 bytes of a text key, PKCS#7 padding.
-- db only calls staticEncrypt when the group's <GROUP>_AES_KEY / <GROUP>_AES_IV are missing from .env.
-- Where the server keeps the key is up to you; this example reads custom settings:
--   ALTER DATABASE app SET app.aes_key = '...';  ALTER DATABASE app SET app.aes_iv = '<32 hex chars>';
CREATE EXTENSION IF NOT EXISTS pgcrypto;

CREATE OR REPLACE FUNCTION staticEncrypt(plain text) RETURNS text
LANGUAGE sql STABLE AS $$
  SELECT encode(decode(current_setting('app.aes_iv'), 'hex'), 'hex') || '^|^' ||
         encode(encrypt_iv(convert_to(plain, 'UTF-8'), current_setting('app.aes_key')::bytea,
                           decode(current_setting('app.aes_iv'), 'hex'), 'aes'), 'hex')
$$;

CREATE OR REPLACE FUNCTION staticDecrypt(enc text) RETURNS text
LANGUAGE sql STABLE AS $$
  SELECT convert_from(decrypt_iv(decode(split_part(enc, '^|^', 2), 'hex'), current_setting('app.aes_key')::bytea,
                                 decode(split_part(enc, '^|^', 1), 'hex'), 'aes'), 'UTF-8')
$$;
