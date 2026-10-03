-- =============================================================
-- Forward-fix migration. Do NOT edit 001-018 (see 005's header).
--
-- departments.instructions (ADR-0029): a few lines an administrator writes
-- for a department — tone, wording, conventions — added to the model's
-- prompt for answers built on that department's documents; the default
-- department's apply to every answer. Plain text, read under RLS with the
-- question; at most 1000 characters, so it always fits the model's
-- context next to the sources.
--
-- Written only on admin_pool (cmd_set_department_instructions); app_user
-- keeps no UPDATE on departments (migration 012), checked by
-- rls_validation section 9.
-- =============================================================

ALTER TABLE departments ADD COLUMN IF NOT EXISTS instructions TEXT;

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'departments_instructions_length') THEN
        ALTER TABLE departments
            ADD CONSTRAINT departments_instructions_length CHECK (char_length(instructions) <= 1000);
    END IF;
END$$;

-- The company's instructions — the default department's — apply to every
-- answer, also for someone an administrator took out of that department:
-- under RLS its row is visible to members only. The default department is
-- everyone's by definition (ADR-0016), so this function hands out exactly
-- its name and instructions, nothing else, to app_user.
CREATE OR REPLACE FUNCTION company_instructions()
RETURNS TABLE (id UUID, name TEXT, instructions TEXT)
LANGUAGE sql
STABLE
SECURITY DEFINER
SET search_path = public, pg_temp
AS $$
    SELECT d.id, d.name, d.instructions
    FROM departments d
    WHERE d.is_default AND d.deleted_at IS NULL AND btrim(coalesce(d.instructions, '')) <> ''
$$;

REVOKE ALL ON FUNCTION company_instructions() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION company_instructions() TO app_user;
