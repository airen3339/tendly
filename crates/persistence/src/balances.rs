//! Balance documents are kept separate from immutable session configurations.
use super::*;

impl Store {
    pub fn endpoint_balance(&self, id: Uuid) -> Result<Option<String>> {
        self.connection
            .lock()
            .expect("sqlite lock")
            .query_row(
                "SELECT document FROM endpoint_balances WHERE profile_id=?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .optional()
    }

    pub fn set_endpoint_balance(&self, id: Uuid, document: &str) -> Result<()> {
        self.connection.lock().expect("sqlite lock").execute(
            "INSERT INTO endpoint_balances(profile_id,document) VALUES(?1,?2) ON CONFLICT(profile_id) DO UPDATE SET document=excluded.document",
            params![id.to_string(), document],
        )?;
        Ok(())
    }

    /// A result from an older script must not overwrite a newly saved configuration.
    pub fn update_endpoint_balance_result(
        &self,
        id: Uuid,
        before: &str,
        after: &str,
    ) -> Result<bool> {
        Ok(self.connection.lock().expect("sqlite lock").execute(
            "UPDATE endpoint_balances SET document=?1 WHERE profile_id=?2 AND document=?3",
            params![after, id.to_string(), before],
        )? == 1)
    }
}
