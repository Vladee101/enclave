-- =============================================================
-- Forward-fix migration. Do NOT edit 001-019 (see 005's header).
--
-- Chat history (ADR-0030): each user's chats, kept until the user
-- deletes them, visible to that user only — RLS on every statement, no
-- exception for administrators in the app.
--
-- An answer quotes documents, so history must not outlive access to them:
--   * document_ids lists every document an answer rests on (its sources,
--     the table of a calculation). Reading a conversation hides an answer
--     with any of them invisible to the reader now (deleted, or a
--     department left) — checked under RLS at read time (chat.rs);
--   * deleting a document (ADR-0015) erases the text of every answer that
--     cites it, in the database, by the trigger below — whoever's it is.
--
-- app_user gets exactly the writes the app makes (ADR-0018): create and
-- delete its chats, rename them, add messages. Messages are never
-- edited by the app; only the trigger (as the owner) redacts them.
--
-- Named chats / chat_messages, not conversations / messages: databases
-- once hand-built from db/schema.sql have empty tables by those names, of
-- another shape, and CREATE TABLE IF NOT EXISTS would silently keep them.
-- =============================================================

CREATE TABLE IF NOT EXISTS chats (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id     UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    title       TEXT NOT NULL CHECK (char_length(title) BETWEEN 1 AND 200),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS chat_messages (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    chat_id UUID NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
    -- The owner again: RLS on messages without a join per row.
    user_id         UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    position        INT  NOT NULL,
    role            TEXT NOT NULL CHECK (role IN ('user', 'assistant')),
    -- NULL once redacted (a cited document was deleted).
    content         TEXT,
    -- Assistant: [{document_id, filename, excerpt, score}] as shown.
    sources         JSONB NOT NULL DEFAULT '[]',
    calculation     TEXT,
    clarification   BOOLEAN NOT NULL DEFAULT false,
    -- Every document the answer rests on; for a question, those it was
    -- limited to in the chat panel.
    document_ids    UUID[] NOT NULL DEFAULT '{}',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (chat_id, position)
);

-- A message's owner is its conversation's owner.
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'chats_id_user_key') THEN
        ALTER TABLE chats ADD CONSTRAINT chats_id_user_key UNIQUE (id, user_id);
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'chat_messages_owner_matches_chat') THEN
        ALTER TABLE chat_messages ADD CONSTRAINT chat_messages_owner_matches_chat
            FOREIGN KEY (chat_id, user_id) REFERENCES chats (id, user_id) ON DELETE CASCADE;
    END IF;
END$$;

CREATE INDEX IF NOT EXISTS idx_chats_user_updated ON chats (user_id, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_chat_messages_documents ON chat_messages USING gin (document_ids);

ALTER TABLE chats ENABLE ROW LEVEL SECURITY;
ALTER TABLE chats FORCE  ROW LEVEL SECURITY;
ALTER TABLE chat_messages ENABLE ROW LEVEL SECURITY;
ALTER TABLE chat_messages FORCE  ROW LEVEL SECURITY;

DROP POLICY IF EXISTS chats_owner ON chats;
CREATE POLICY chats_owner ON chats
    USING (user_id = (SELECT current_user_id()))
    WITH CHECK (user_id = (SELECT current_user_id()));

DROP POLICY IF EXISTS chat_messages_owner ON chat_messages;
CREATE POLICY chat_messages_owner ON chat_messages
    USING (user_id = (SELECT current_user_id()))
    WITH CHECK (user_id = (SELECT current_user_id()));

REVOKE ALL ON chats, chat_messages FROM app_user;
GRANT SELECT, INSERT, DELETE ON chats TO app_user;
GRANT UPDATE (title, updated_at) ON chats TO app_user;
GRANT SELECT, INSERT ON chat_messages TO app_user;

-- Deleting a document erases the answers that cite it (ADR-0015, 0030).
CREATE OR REPLACE FUNCTION redact_chat_messages()
RETURNS TRIGGER
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = public, pg_temp
AS $$
BEGIN
    IF NEW.deleted_at IS NOT NULL AND OLD.deleted_at IS NULL THEN
        UPDATE chat_messages
        SET content = NULL, sources = '[]', calculation = NULL
        WHERE role = 'assistant' AND document_ids @> ARRAY[NEW.id];
    END IF;
    RETURN NEW;
END;
$$;

REVOKE ALL ON FUNCTION redact_chat_messages() FROM PUBLIC;

DROP TRIGGER IF EXISTS documents_redact_chat_messages ON documents;
CREATE TRIGGER documents_redact_chat_messages
    AFTER UPDATE OF deleted_at ON documents
    FOR EACH ROW EXECUTE FUNCTION redact_chat_messages();
