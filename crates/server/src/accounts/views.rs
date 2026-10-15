//! An account as the API shows it, with the last usage reading cached on disk.

use super::*;

pub(super) fn initial_view(state: &AppState, record: &AccountRecord) -> Result<AccountView> {
    let metadata_directory = directory(state, record.id)?;
    let directory = record
        .native_config
        .as_ref()
        .map(|reference| PathBuf::from(&reference.config_dir))
        .unwrap_or_else(|| metadata_directory.clone());
    let client = crate::adapters::agent(record.provider)?;
    let operations = client.account_operations();
    // Where this account's sign-in has to land: wherever the configuration it
    // references already points (for the host default, no override at all),
    // or else the directory the account owns.
    let login_home = match record.native_config.as_ref() {
        Some(reference) => reference.config_env.clone(),
        None => Some(metadata_directory.to_string_lossy().into_owned()),
    };
    let guidance = client.login_guidance(login_home.as_deref());
    Ok(AccountView {
        id: record.id,
        name: record.name.clone(),
        provider: record.provider,
        profile_id: record.profile_id,
        native_source_id: record
            .native_config
            .as_ref()
            .map(|reference| reference.source_id.clone()),
        storage_path: directory.to_string_lossy().into_owned(),
        // An account Tendly created owns its directory; one that references a
        // configuration the host already had does not.
        shared_configuration: record.native_config.is_some(),
        proxy_url: record.proxy_url.clone(),
        status: "unknown".into(),
        email: None,
        plan: None,
        quota_tier: None,
        checked_at: None,
        error: None,
        login: None,
        limits: None,
        capabilities: Capabilities {
            login: operations.managed_login,
            refresh_token: operations.managed_login,
            logout: operations.managed_login,
            usage: operations.usage,
            ..Default::default()
        },
        guidance,
        reset_outcome: None,
        usage: None,
    })
}
/// A successful usage reading, kept in Tendly's own account directory. The
/// provider allows very few checks, so re-reading this costs nothing and lets
/// the page show figures without spending an allowance. It is never written
/// into the native client's configuration directory.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct CachedUsage {
    pub(super) limits: Limits,
    /// When the figures were actually read, so they are never shown as live.
    pub(super) checked_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) source: Option<String>,
}
pub(super) fn usage_cache_path(state: &AppState, id: Uuid) -> Result<PathBuf> {
    Ok(directory(state, id)?.join("usage.json"))
}
pub(super) fn read_usage_cache(state: &AppState, id: Uuid) -> Option<CachedUsage> {
    let path = usage_cache_path(state, id).ok()?;
    if fs::metadata(&path).ok()?.len() > 64 * 1024 {
        return None;
    }
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}
pub(super) fn write_usage_cache(state: &AppState, id: Uuid, cached: &CachedUsage) {
    // A cache that cannot be written is not worth failing a good reading over.
    if let Ok(path) = usage_cache_path(state, id)
        && let Ok(bytes) = serde_json::to_vec(cached)
    {
        let _ = fs::write(path, bytes);
    }
}
pub(super) fn view(state: &AppState, id: Uuid) -> Result<AccountView> {
    let record = record(state, id)?;
    let cached = state
        .accounts
        .inner
        .lock()
        .expect("accounts lock")
        .views
        .get(&id)
        .cloned();
    let mut view = cached.map_or_else(|| initial_view(state, &record), Ok)?;
    if let Some(profile) = state
        .store
        .get_endpoint_profile(record.profile_id)
        .map_err(ApiError::internal)?
    {
        view.name = profile.name;
    }
    // Figures the user already paid an allowance for survive a status refresh
    // and a server restart, but always arrive labelled with when they were read.
    if view
        .limits
        .as_ref()
        .is_none_or(|limits| limits.primary.is_none() && limits.secondary.is_none())
        && let Some(cached) = read_usage_cache(state, id)
    {
        view.limits = Some(cached.limits);
        let usage = view.usage.get_or_insert_with(|| UsageView {
            availability: "cached".into(),
            capture_path: None,
            captured_at: None,
            source: None,
            retry_after_seconds: None,
            retained_checked_at: None,
        });
        usage.source = cached.source;
        usage.retained_checked_at = Some(cached.checked_at);
    }
    Ok(view)
}
