//! The user's preferences, stored as one JSON document.

use super::*;

impl Store {
    pub fn preferences(&self) -> Result<serde_json::Value> {
        let raw: Option<String> = self
            .connection
            .lock()
            .expect("sqlite lock")
            .query_row("SELECT value FROM preferences WHERE id=1", [], |r| r.get(0))
            .optional()?;
        match raw {
            Some(raw) => serde_json::from_str(&raw).map_err(conversion_error),
            None => Ok(serde_json::Value::Null),
        }
    }
    pub fn set_preferences(&self, value: &serde_json::Value) -> Result<()> {
        let encoded = serde_json::to_string(value).map_err(conversion_error)?;
        self.connection.lock().expect("sqlite lock").execute(
            "INSERT INTO preferences(id,value,updated_at) VALUES (1,?1,?2) ON CONFLICT(id) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at",
            params![encoded, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }
}
