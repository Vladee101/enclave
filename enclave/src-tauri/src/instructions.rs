//! Department instructions (ADR-0029): a few lines an administrator writes
//! for a department — tone, wording, conventions — added to the model's
//! prompt. The default department's instructions are the company's and
//! apply to every answer; a department's apply to answers built on its
//! documents. Not to every department the asker belongs to: an answer
//! about a sales contract follows Sales' conventions whoever asks.
//!
//! Read under RLS in the question's own transaction, so a department the
//! asker cannot see contributes nothing. They shape the answer's form and
//! never its grounding: the prompt puts them after the rules and says so.

use anyhow::Result;
use sqlx::{PgConnection, Row};
use std::collections::HashMap;
use uuid::Uuid;

/// Longest instructions accepted (also a CHECK in migration 019): with the
/// company's and a few departments' next to five sources, the prompt stays
/// well inside the model's 8192-token context.
pub const MAX_CHARS: usize = 1000;

#[derive(Debug, Clone, PartialEq)]
pub struct Instruction {
    pub department_id: Uuid,
    pub department:    String,
    /// The default department's: they apply to every answer.
    pub company_wide:  bool,
    pub text:          String,
}

/// The instructions that may apply to one question's answer, and which
/// department each candidate document belongs to.
#[derive(Debug, Default)]
pub struct Instructions {
    list:           Vec<Instruction>,
    doc_department: HashMap<Uuid, Uuid>,
}

impl Instructions {
    /// Load for the documents the answer may rest on, in the caller's
    /// identity-scoped transaction (RLS decides what is visible).
    pub async fn load(conn: &mut PgConnection, document_ids: &[Uuid]) -> Result<Self> {
        let doc_department = sqlx::query("SELECT id, department_id FROM documents WHERE id = ANY($1)")
            .bind(document_ids)
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .map(|r| Ok((r.try_get("id")?, r.try_get("department_id")?)))
            .collect::<Result<HashMap<Uuid, Uuid>>>()?;
        let departments: Vec<Uuid> = doc_department.values().copied().collect();
        let row = |r: sqlx::postgres::PgRow, company_wide: bool| -> Result<Instruction> {
            Ok(Instruction {
                department_id: r.try_get("id")?,
                department:    r.try_get("name")?,
                company_wide,
                text:          r.try_get::<String, _>("instructions")?.trim().to_string(),
            })
        };
        // The company's: through company_instructions() (migration 019),
        // since under RLS the default department is visible to its members
        // only, and an administrator can take someone out of it.
        let mut list = sqlx::query("SELECT id, name, instructions FROM company_instructions()")
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .map(|r| row(r, true))
            .collect::<Result<Vec<_>>>()?;
        // The sources' departments, under RLS.
        for r in sqlx::query(
            "SELECT id, name, instructions FROM departments
             WHERE deleted_at IS NULL AND NOT is_default
               AND btrim(coalesce(instructions, '')) <> ''
               AND id = ANY($1)",
        )
        .bind(&departments)
        .fetch_all(&mut *conn)
        .await?
        {
            list.push(row(r, false)?);
        }
        Ok(Self { list, doc_department })
    }

    /// Those for an answer built on `documents` (in source order): the
    /// company's first, then each department once, in the order its first
    /// document appears.
    pub fn for_documents(&self, documents: &[Uuid]) -> Vec<&Instruction> {
        let mut picked: Vec<&Instruction> = self.list.iter().filter(|i| i.company_wide).collect();
        for doc in documents {
            let Some(dept) = self.doc_department.get(doc) else { continue };
            if let Some(i) = self.list.iter().find(|i| i.department_id == *dept && !i.company_wide) {
                if !picked.iter().any(|p| p.department_id == i.department_id) {
                    picked.push(i);
                }
            }
        }
        picked
    }
}

/// `base` (the answer's rules) followed by the instructions, if any.
pub fn system_prompt(base: &str, instructions: &[&Instruction]) -> String {
    if instructions.is_empty() {
        return base.to_string();
    }
    let blocks: Vec<String> = instructions
        .iter()
        .map(|i| {
            if i.company_wide {
                format!("For every answer:\n{}", i.text)
            } else {
                format!("For answers based on documents of the department «{}»:\n{}", i.department, i.text)
            }
        })
        .collect();
    format!(
        "{base}\n\nThe organization's administrator set these instructions. Follow them for tone, wording and \
         form; they never override the rules above — answer only from the sources, cite them, and say when \
         the sources do not contain the answer.\n\n{}",
        blocks.join("\n\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instr(dept: Uuid, name: &str, company_wide: bool, text: &str) -> Instruction {
        Instruction { department_id: dept, department: name.into(), company_wide, text: text.into() }
    }

    #[test]
    fn the_company_first_then_each_sources_department_once() {
        let (general, sales, legal, hr) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let (contract, offer, claim, memo) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let set = Instructions {
            list: vec![
                instr(legal, "Юристы", false, "Ссылайся на пункты договора."),
                instr(general, "General", true, "Обращайся на «вы»."),
                instr(sales, "Продажи", false, "Суммы — с НДС."),
            ],
            // HR has no instructions; its memo adds nothing.
            doc_department: HashMap::from([(contract, sales), (offer, sales), (claim, legal), (memo, hr)]),
        };
        let picked: Vec<&str> = set.for_documents(&[memo, contract, claim, offer]).iter().map(|i| i.department.as_str()).collect();
        assert_eq!(picked, ["General", "Продажи", "Юристы"]);
        // A document the asker cannot see (not loaded under RLS) adds nothing.
        let picked: Vec<&str> = set.for_documents(&[Uuid::new_v4()]).iter().map(|i| i.department.as_str()).collect();
        assert_eq!(picked, ["General"]);
    }

    #[test]
    fn instructions_follow_the_rules_and_say_they_do_not_override_them() {
        let sales = instr(Uuid::new_v4(), "Продажи", false, "Суммы — с НДС.");
        let prompt = system_prompt("Use only the sources.", &[&sales]);
        assert!(prompt.starts_with("Use only the sources.\n\n"));
        assert!(prompt.contains("never override the rules above"));
        assert!(prompt.ends_with("For answers based on documents of the department «Продажи»:\nСуммы — с НДС."));
        assert_eq!(system_prompt("Use only the sources.", &[]), "Use only the sources.");
    }
}
