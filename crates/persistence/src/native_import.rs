//! Sessions imported from a client's own history.

use super::*;

impl Store {
    pub fn import_native_session(
        &self,
        workspace_id: WorkspaceId,
        provider: ProviderKind,
        title: &str,
        source_id: &str,
        native_id: &str,
        config_dir: &str,
    ) -> Result<Session> {
        self.import_native_session_with_environment(
            workspace_id,
            provider,
            title,
            source_id,
            native_id,
            config_dir,
            None,
        )?
        .ok_or(rusqlite::Error::InvalidQuery)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn import_native_session_with_environment(
        &self,
        workspace_id: WorkspaceId,
        provider: ProviderKind,
        title: &str,
        source_id: &str,
        native_id: &str,
        config_dir: &str,
        environment: Option<EnvironmentOverrides>,
    ) -> Result<Option<Session>> {
        if let Some(environment) = &environment {
            tendly_domain::validate_environment(environment)
                .map_err(|_| rusqlite::Error::InvalidQuery)?;
        }
        let mut conn = self.connection.lock().expect("sqlite lock");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing)=tx.query_row(&format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE workspace_id=?1 AND native_source_id=?2 AND provider_session_id=?3 AND resume_source_id IS NULL"),params![workspace_id.to_string(),source_id,native_id],session_row).optional()? {
            if environment.as_ref().is_some_and(|requested| *requested != existing.environment) { return Ok(None); }
            return Ok(Some(existing));
        }
        let id = Uuid::new_v4();
        let now = Utc::now().to_rfc3339();
        let encoded =
            serde_json::to_string(&environment.unwrap_or_default()).map_err(conversion_error)?;
        tx.execute("INSERT INTO sessions (id,workspace_id,provider,title,status,created_at,updated_at,provider_session_id,native_source_id,native_config_dir,environment) VALUES (?1,?2,?3,?4,'stopped',?5,?5,?6,?7,?8,?9)",params![id.to_string(),workspace_id.to_string(),provider_name(&provider),title,now,native_id,source_id,config_dir,encoded])?;
        let record = tx.query_row(
            &format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE id=?1"),
            [id.to_string()],
            session_row,
        )?;
        tx.commit()?;
        Ok(Some(record))
    }
}
