//! Workspace layouts, and the shared canvas, which is written with a
//! compare-and-swap on its revision so concurrent writers cannot overwrite
//! each other.

use super::*;

impl Store {
    pub fn get_layout(&self, id: WorkspaceId) -> Result<Option<String>> {
        self.connection
            .lock()
            .expect("sqlite lock")
            .query_row(
                "SELECT layout_json FROM layouts WHERE workspace_id=?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .optional()
    }

    pub fn save_layout(&self, id: WorkspaceId, json: &str) -> Result<()> {
        self.connection.lock().expect("sqlite lock").execute("INSERT INTO layouts (workspace_id,layout_json,updated_at) VALUES (?1,?2,?3) ON CONFLICT(workspace_id) DO UPDATE SET layout_json=excluded.layout_json,updated_at=excluded.updated_at", params![id.to_string(),json,Utc::now().to_rfc3339()])?;
        Ok(())
    }

    /// Read only the singleton canvas. Legacy per-workspace layouts are never
    /// inspected or implicitly migrated by this operation.
    pub fn get_shared_canvas_layout(&self) -> Result<SharedCanvasLayout> {
        self.connection.lock().expect("sqlite lock").query_row(
            "SELECT layout_json,revision FROM shared_canvas WHERE singleton=1",
            [],
            |row| {
                Ok(SharedCanvasLayout {
                    layout: row.get(0)?,
                    revision: u64::try_from(row.get::<_, i64>(1)?).map_err(conversion_error)?,
                })
            },
        )
    }

    /// Compare-and-swap across tabs and independent SQLite connections. None
    /// means the caller's revision is stale; neither layout nor timestamp changes.
    pub fn save_shared_canvas_layout(
        &self,
        layout: &str,
        expected_revision: u64,
    ) -> Result<Option<u64>> {
        // SQLite stores signed 64-bit integers. Out-of-range expectations can
        // never match a writable revision, so report a normal CAS conflict.
        let Some(next_revision) = expected_revision
            .checked_add(1)
            .and_then(|value| i64::try_from(value).ok())
        else {
            return Ok(None);
        };
        let mut conn = self.connection.lock().expect("sqlite lock");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE shared_canvas SET layout_json=?1,revision=?2,updated_at=?3 WHERE singleton=1 AND revision=?4",
            params![layout,next_revision,Utc::now().to_rfc3339(),expected_revision as i64],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        tx.commit()?;
        Ok(Some(next_revision as u64))
    }
}
