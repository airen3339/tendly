//! Pinning a native client configuration to an account, and the isolated
//! environment a session or login runs the client in.

use super::*;

pub fn pin(state: &AppState, source_id: &str) -> Result<(ProviderKind, NativeConfigReference)> {
    let id = source_id
        .strip_prefix("account:")
        .and_then(|id| Uuid::parse_str(id).ok())
        .ok_or_else(|| ApiError::bad("Invalid account source"))?;
    let record = record(state, id)?;
    if let Some(reference) = record.native_config {
        // Re-pin the host source on every use so an unavailable or changed
        // native directory cannot silently become an account configuration.
        let (provider, current) = crate::native_config::pin(state, &reference.source_id)?;
        if provider != record.provider || current != reference {
            return Err(ApiError::conflict(
                "Native account configuration identity changed",
            ));
        }
        return Ok((provider, current));
    }
    let config_dir = directory(state, id)?
        .to_str()
        .ok_or_else(|| ApiError::bad("Account directory must be UTF-8"))?
        .to_owned();
    Ok((
        record.provider,
        NativeConfigReference {
            source_id: format!("account:{id}"),
            config_env: Some(config_dir.clone()),
            config_dir,
        },
    ))
}
pub fn validate_reference(
    state: &AppState,
    provider: &ProviderKind,
    reference: &NativeConfigReference,
) -> Result<PathBuf> {
    let (actual_provider, actual) = pin(state, &reference.source_id)?;
    if actual_provider != *provider || actual != *reference {
        return Err(ApiError::conflict("Account configuration identity changed"));
    }
    Ok(PathBuf::from(actual.config_dir))
}
pub(super) fn isolated_environment(
    provider: &ProviderKind,
    reference: &NativeConfigReference,
) -> (BTreeMap<String, String>, Vec<String>) {
    let key = crate::adapters::adapter(*provider).map_or("", |client| client.config_key());
    let environment = reference
        .config_env
        .as_ref()
        .map(|config_env| [(key.into(), config_env.clone())].into())
        .unwrap_or_default();
    // Every client's variables, not only this one's: a key meant for one can
    // still reach another through a shared gateway or SDK.
    let prefixes: Vec<&str> = std::iter::once("TENDLY_")
        .chain(
            crate::adapters::all()
                .flat_map(|client| client.account_prefixes())
                .copied(),
        )
        .collect();
    let remove = env::vars_os()
        .filter_map(|(key, _)| {
            let text = key.to_string_lossy().into_owned();
            let upper = text.to_ascii_uppercase();
            (prefixes.iter().any(|prefix| upper.starts_with(prefix)) || upper == "CLAUDECODE")
                .then_some(text)
        })
        .filter(|existing| existing != key)
        .collect();
    (environment, remove)
}
pub fn build(
    state: &AppState,
    provider: &ProviderKind,
    reference: &NativeConfigReference,
    cwd: PathBuf,
) -> Result<SpawnSpec> {
    validate_reference(state, provider, reference)?;
    let id = Uuid::parse_str(reference.source_id.trim_start_matches("account:"))
        .map_err(|_| ApiError::bad("Invalid account source"))?;
    let inner = state.accounts.inner.lock().expect("accounts lock");
    if inner.pending.contains_key(&id) || inner.maintenance.contains(&id) {
        return Err(ApiError::conflict(
            "This account is being maintained; wait for its account operation to finish",
        ));
    }
    drop(inner);
    let mut spec = account_spec(&state.state_dir, provider, reference, cwd);
    // What the client adds to every launch: for Claude Code, its opt-in usage
    // capture, a fallback to the explicit usage query the account page prefers.
    let (args, extras) =
        crate::adapters::agent(*provider)?.launch_extras(state, reference.config_env.as_deref());
    spec.args.extend(args);
    spec.env.extend(extras);
    spec.env_remove.retain(|key| !spec.env.contains_key(key));
    Ok(spec)
}
pub(super) fn account_spec(
    state_dir: &Path,
    provider: &ProviderKind,
    reference: &NativeConfigReference,
    cwd: PathBuf,
) -> SpawnSpec {
    let (environment, remove) = isolated_environment(provider, reference);
    let program = crate::clients::program(state_dir, provider);
    SpawnSpec {
        program,
        args: vec![],
        cwd,
        env: environment,
        env_remove: remove,
    }
}
