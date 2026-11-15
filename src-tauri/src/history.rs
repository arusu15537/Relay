//! Run history in SQLite (`%LOCALAPPDATA%\Relay\history.db`).
//!
//! A *job* is one instruction + spec. Each local-model execution of that spec is a *run*
//! (so "re-run with another model" adds runs to the same job). Each run stores its full
//! result (raw model outputs, validation, timing) and per-file proposals with accept/reject
//! decisions.

use crate::pipeline::ImplementResult;
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;

pub struct History {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct JobRecord {
    pub id: String,
    pub created_at: String,
    pub project_root: String,
    pub instruction: String,
    pub spec: String,
    pub spec_edited: bool,
    pub claude_backend: String,
    pub claude_model: String,
    pub expand_ms: Option<i64>,
    pub expand_cost: Option<f64>,
    pub context_files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RunFileRecord {
    pub path: String,
    pub original: Option<String>,
    pub proposed: String,
    pub decision: String,
    pub decided_at: Option<String>,
    pub final_content: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RunRecord {
    pub id: String,
    pub job_id: String,
    pub created_at: String,
    pub profile_id: String,
    pub profile_name: String,
    pub model_file: String,
    pub mode: String,
    pub spec: String,
    pub status: String,
    pub validation_ok: bool,
    pub duration_ms: i64,
    pub avg_tok_per_sec: Option<f64>,
    pub error: Option<String>,
    pub result: Option<serde_json::Value>,
    pub review: Option<serde_json::Value>,
    pub files: Vec<RunFileRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobSummary {
    pub id: String,
    pub created_at: String,
    pub project_root: String,
    pub instruction: String,
    pub runs: Vec<RunSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunSummary {
    pub id: String,
    pub profile_name: String,
    pub status: String,
    pub validation_ok: bool,
    pub avg_tok_per_sec: Option<f64>,
    pub duration_ms: i64,
    pub accepted: i64,
    pub rejected: i64,
    pub files: i64,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS jobs (
  id TEXT PRIMARY KEY,
  created_at TEXT NOT NULL,
  project_root TEXT NOT NULL,
  instruction TEXT NOT NULL,
  spec TEXT NOT NULL DEFAULT '',
  spec_edited INTEGER NOT NULL DEFAULT 0,
  claude_backend TEXT NOT NULL DEFAULT '',
  claude_model TEXT NOT NULL DEFAULT '',
  expand_ms INTEGER,
  expand_cost REAL,
  context_files TEXT NOT NULL DEFAULT '[]'
);
CREATE TABLE IF NOT EXISTS runs (
  id TEXT PRIMARY KEY,
  job_id TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
  created_at TEXT NOT NULL,
  profile_id TEXT NOT NULL,
  profile_name TEXT NOT NULL,
  model_file TEXT NOT NULL DEFAULT '',
  mode TEXT NOT NULL DEFAULT '',
  spec TEXT NOT NULL DEFAULT '',
  status TEXT NOT NULL,
  validation_ok INTEGER NOT NULL DEFAULT 0,
  duration_ms INTEGER NOT NULL DEFAULT 0,
  avg_tok_per_sec REAL,
  error TEXT,
  result_json TEXT,
  review_json TEXT
);
CREATE TABLE IF NOT EXISTS run_files (
  run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
  path TEXT NOT NULL,
  original TEXT,
  proposed TEXT NOT NULL,
  decision TEXT NOT NULL DEFAULT 'pending',
  decided_at TEXT,
  final_content TEXT,
  PRIMARY KEY (run_id, path)
);
CREATE INDEX IF NOT EXISTS runs_job ON runs(job_id);
"#;

fn now() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

impl History {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn upsert_job(&self, j: &JobRecord) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO jobs (id, created_at, project_root, instruction, spec, spec_edited, claude_backend, claude_model, expand_ms, expand_cost, context_files)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(id) DO UPDATE SET project_root=excluded.project_root, instruction=excluded.instruction, spec=excluded.spec,
               spec_edited=excluded.spec_edited, claude_backend=excluded.claude_backend, claude_model=excluded.claude_model,
               expand_ms=excluded.expand_ms, expand_cost=excluded.expand_cost, context_files=excluded.context_files",
            params![
                j.id,
                if j.created_at.is_empty() { now() } else { j.created_at.clone() },
                j.project_root,
                j.instruction,
                j.spec,
                j.spec_edited as i64,
                j.claude_backend,
                j.claude_model,
                j.expand_ms,
                j.expand_cost,
                serde_json::to_string(&j.context_files)?
            ],
        )?;
        Ok(())
    }

    /// Create the job row if it doesn't exist yet (e.g. a spec typed by hand, no expansion).
    pub fn ensure_job(&self, id: &str, project_root: &str, instruction: &str) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT OR IGNORE INTO jobs (id, created_at, project_root, instruction) VALUES (?1, ?2, ?3, ?4)",
            params![id, now(), project_root, instruction],
        )?;
        Ok(())
    }

    pub fn update_job_spec(&self, id: &str, spec: &str, context_files: &[String]) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "UPDATE jobs SET spec_edited = CASE WHEN spec != '' AND spec != ?2 THEN 1 ELSE spec_edited END, spec=?2, context_files=?3 WHERE id=?1",
            params![id, spec, serde_json::to_string(context_files)?],
        )?;
        Ok(())
    }

    pub fn save_run(&self, job_id: &str, spec: &str, model_file: &str, r: &ImplementResult) -> Result<()> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        let status = if r.cancelled {
            "cancelled"
        } else if r.validation_ok {
            "ok"
        } else if r.error.is_some() && r.files.is_empty() {
            "error"
        } else {
            "failed_validation"
        };
        tx.execute(
            "INSERT OR REPLACE INTO runs (id, job_id, created_at, profile_id, profile_name, model_file, mode, spec, status, validation_ok, duration_ms, avg_tok_per_sec, error, result_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                r.run_id,
                job_id,
                now(),
                r.profile_id,
                r.profile_name,
                model_file,
                serde_json::to_value(r.mode)?.as_str().unwrap_or(""),
                spec,
                status,
                r.validation_ok as i64,
                r.duration_ms as i64,
                r.avg_tok_per_sec,
                r.error,
                serde_json::to_string(r)?
            ],
        )?;
        for f in &r.files {
            tx.execute(
                "INSERT OR REPLACE INTO run_files (run_id, path, original, proposed, decision) VALUES (?1, ?2, ?3, ?4, 'pending')",
                params![r.run_id, f.path, f.original, f.proposed],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn set_decision(&self, run_id: &str, path: &str, decision: &str, final_content: Option<&str>) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "UPDATE run_files SET decision=?3, decided_at=?4, final_content=?5 WHERE run_id=?1 AND path=?2",
            params![run_id, path, decision, now(), final_content],
        )?;
        Ok(())
    }

    pub fn save_review(&self, run_id: &str, review: &serde_json::Value) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "UPDATE runs SET review_json=?2 WHERE id=?1",
            params![run_id, serde_json::to_string(review)?],
        )?;
        Ok(())
    }

    pub fn list_jobs(&self, limit: usize, query: &str) -> Result<Vec<JobSummary>> {
        let c = self.conn.lock().unwrap();
        let like = format!("%{}%", query.trim());
        let mut stmt = c.prepare(
            "SELECT id, created_at, project_root, instruction FROM jobs
             WHERE instruction LIKE ?1 OR project_root LIKE ?1 OR spec LIKE ?1
             ORDER BY created_at DESC LIMIT ?2",
        )?;
        let jobs: Vec<(String, String, String, String)> = stmt
            .query_map(params![like, limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut out = Vec::new();
        let mut rs = c.prepare(
            "SELECT r.id, r.profile_name, r.status, r.validation_ok, r.avg_tok_per_sec, r.duration_ms,
                    (SELECT COUNT(*) FROM run_files f WHERE f.run_id=r.id AND f.decision='accepted'),
                    (SELECT COUNT(*) FROM run_files f WHERE f.run_id=r.id AND f.decision='rejected'),
                    (SELECT COUNT(*) FROM run_files f WHERE f.run_id=r.id)
             FROM runs r WHERE r.job_id=?1 ORDER BY r.created_at",
        )?;
        for (id, created_at, project_root, instruction) in jobs {
            let runs = rs
                .query_map(params![id], |r| {
                    Ok(RunSummary {
                        id: r.get(0)?,
                        profile_name: r.get(1)?,
                        status: r.get(2)?,
                        validation_ok: r.get::<_, i64>(3)? != 0,
                        avg_tok_per_sec: r.get(4)?,
                        duration_ms: r.get(5)?,
                        accepted: r.get(6)?,
                        rejected: r.get(7)?,
                        files: r.get(8)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?;
            out.push(JobSummary {
                id,
                created_at,
                project_root,
                instruction,
                runs,
            });
        }
        Ok(out)
    }

    pub fn get_job(&self, id: &str) -> Result<Option<(JobRecord, Vec<RunRecord>)>> {
        let c = self.conn.lock().unwrap();
        let job = c
            .query_row(
                "SELECT id, created_at, project_root, instruction, spec, spec_edited, claude_backend, claude_model, expand_ms, expand_cost, context_files FROM jobs WHERE id=?1",
                params![id],
                |r| {
                    Ok(JobRecord {
                        id: r.get(0)?,
                        created_at: r.get(1)?,
                        project_root: r.get(2)?,
                        instruction: r.get(3)?,
                        spec: r.get(4)?,
                        spec_edited: r.get::<_, i64>(5)? != 0,
                        claude_backend: r.get(6)?,
                        claude_model: r.get(7)?,
                        expand_ms: r.get(8)?,
                        expand_cost: r.get(9)?,
                        context_files: serde_json::from_str(&r.get::<_, String>(10)?).unwrap_or_default(),
                    })
                },
            )
            .optional()?;
        let Some(job) = job else { return Ok(None) };
        let mut stmt = c.prepare(
            "SELECT id, job_id, created_at, profile_id, profile_name, model_file, mode, spec, status, validation_ok, duration_ms, avg_tok_per_sec, error, result_json, review_json
             FROM runs WHERE job_id=?1 ORDER BY created_at",
        )?;
        let mut runs: Vec<RunRecord> = stmt
            .query_map(params![id], |r| {
                Ok(RunRecord {
                    id: r.get(0)?,
                    job_id: r.get(1)?,
                    created_at: r.get(2)?,
                    profile_id: r.get(3)?,
                    profile_name: r.get(4)?,
                    model_file: r.get(5)?,
                    mode: r.get(6)?,
                    spec: r.get(7)?,
                    status: r.get(8)?,
                    validation_ok: r.get::<_, i64>(9)? != 0,
                    duration_ms: r.get(10)?,
                    avg_tok_per_sec: r.get(11)?,
                    error: r.get(12)?,
                    result: r.get::<_, Option<String>>(13)?.and_then(|s| serde_json::from_str(&s).ok()),
                    review: r.get::<_, Option<String>>(14)?.and_then(|s| serde_json::from_str(&s).ok()),
                    files: vec![],
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut fs =
            c.prepare("SELECT path, original, proposed, decision, decided_at, final_content FROM run_files WHERE run_id=?1 ORDER BY path")?;
        for run in &mut runs {
            run.files = fs
                .query_map(params![run.id], |r| {
                    Ok(RunFileRecord {
                        path: r.get(0)?,
                        original: r.get(1)?,
                        proposed: r.get(2)?,
                        decision: r.get(3)?,
                        decided_at: r.get(4)?,
                        final_content: r.get(5)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?;
        }
        Ok(Some((job, runs)))
    }

    pub fn get_run_file(&self, run_id: &str, path: &str) -> Result<Option<RunFileRecord>> {
        let c = self.conn.lock().unwrap();
        Ok(c.query_row(
            "SELECT path, original, proposed, decision, decided_at, final_content FROM run_files WHERE run_id=?1 AND path=?2",
            params![run_id, path],
            |r| {
                Ok(RunFileRecord {
                    path: r.get(0)?,
                    original: r.get(1)?,
                    proposed: r.get(2)?,
                    decision: r.get(3)?,
                    decided_at: r.get(4)?,
                    final_content: r.get(5)?,
                })
            },
        )
        .optional()?)
    }

    /// (job id, project root, spec) for a run.
    pub fn run_context(&self, run_id: &str) -> Result<Option<(String, String, String)>> {
        let c = self.conn.lock().unwrap();
        Ok(c.query_row(
            "SELECT r.job_id, j.project_root, r.spec FROM runs r JOIN jobs j ON j.id = r.job_id WHERE r.id=?1",
            params![run_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?)
    }

    pub fn run_files(&self, run_id: &str) -> Result<Vec<RunFileRecord>> {
        let c = self.conn.lock().unwrap();
        let mut fs =
            c.prepare("SELECT path, original, proposed, decision, decided_at, final_content FROM run_files WHERE run_id=?1 ORDER BY path")?;
        let rows = fs
            .query_map(params![run_id], |r| {
                Ok(RunFileRecord {
                    path: r.get(0)?,
                    original: r.get(1)?,
                    proposed: r.get(2)?,
                    decision: r.get(3)?,
                    decided_at: r.get(4)?,
                    final_content: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    pub fn delete_job(&self, id: &str) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute("DELETE FROM jobs WHERE id=?1", params![id])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::{BudgetInput, SplitMode, plan};
    use crate::editformat::ProposedFile;

    fn result(run_id: &str, ok: bool) -> ImplementResult {
        ImplementResult {
            run_id: run_id.into(),
            profile_id: "p".into(),
            profile_name: "Qwen".into(),
            mode: SplitMode::Single,
            budget: plan(&BudgetInput {
                context_limit: 1000,
                max_output: 10,
                reasoning_reserve: 0,
                system_tokens: 1,
                spec_tokens: 1,
                files: vec![],
                per_file_spec_tokens: vec![],
                exact: false,
            }),
            files: vec![ProposedFile {
                path: "a.txt".into(),
                original: Some("x".into()),
                proposed: "y".into(),
            }],
            calls: vec![],
            validation_ok: ok,
            cancelled: false,
            error: None,
            warnings: vec![],
            duration_ms: 1234,
            avg_tok_per_sec: Some(42.0),
            workflow: None,
        }
    }

    #[test]
    fn job_run_decision_roundtrip() {
        let h = History::open_in_memory().unwrap();
        let job = JobRecord {
            id: "j1".into(),
            project_root: "D:/p".into(),
            instruction: "add retry".into(),
            spec: "## Summary".into(),
            context_files: vec!["a.txt".into()],
            ..Default::default()
        };
        h.upsert_job(&job).unwrap();
        h.save_run("j1", "## Summary", "q.gguf", &result("r1", true)).unwrap();
        h.save_run("j1", "## Summary", "g.gguf", &result("r2", false)).unwrap();
        h.set_decision("r1", "a.txt", "accepted", Some("y")).unwrap();
        h.save_review("r1", &serde_json::json!({"passed": true})).unwrap();

        let list = h.list_jobs(10, "retry").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].runs.len(), 2);
        assert_eq!(list[0].runs[0].accepted, 1);
        assert_eq!(list[0].runs[1].status, "failed_validation");
        assert!(h.list_jobs(10, "nomatch").unwrap().is_empty());

        let (j, runs) = h.get_job("j1").unwrap().unwrap();
        assert_eq!(j.context_files, vec!["a.txt"]);
        assert_eq!(runs[0].files[0].decision, "accepted");
        assert_eq!(runs[0].review.as_ref().unwrap()["passed"], true);
        assert_eq!(runs[0].result.as_ref().unwrap()["avgTokPerSec"], 42.0);

        h.delete_job("j1").unwrap();
        assert!(h.get_job("j1").unwrap().is_none());
    }
}
