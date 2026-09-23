-- =============================================================
-- Forward-fix migration. Do NOT edit 001-009 (see 005's header).
--
-- A shared default department (ADR-0016). New profiles join it and
-- only it; every other membership is granted by an administrator.
-- Replaces the per-user "<name>'s Department" that profile creation
-- used to make: one department per person defeated the point of
-- departments and invited uploads into the wrong one.
--
-- A department is an access grant (ADR-0008), which is why the new
-- user does not get to choose one: profiles are created from the
-- login screen without authentication.
-- =============================================================

ALTER TABLE departments ADD COLUMN IF NOT EXISTS is_default BOOLEAN NOT NULL DEFAULT false;

CREATE UNIQUE INDEX IF NOT EXISTS uq_departments_single_default
    ON departments (is_default) WHERE is_default;

-- Adopt an existing "General" if there is one (departments.name is
-- UNIQUE); otherwise create it.
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM departments WHERE is_default) THEN
        IF EXISTS (SELECT 1 FROM departments WHERE name = 'General') THEN
            UPDATE departments SET is_default = true WHERE name = 'General';
        ELSE
            INSERT INTO departments (name, slug, is_default) VALUES ('General', 'general', true);
        END IF;
    END IF;
END$$;

-- Everyone who already exists belongs to the shared department too.
-- Existing per-user departments and their documents are left alone:
-- removing a department is a separate decision (what happens to its
-- documents) — see ADR-0016.
INSERT INTO department_members (user_id, department_id)
SELECT u.id, d.id
FROM users u CROSS JOIN departments d
WHERE d.is_default
ON CONFLICT DO NOTHING;
