//! Running an account operation through the native bridge and applying the
//! events it reports.

use super::*;

pub(super) async fn run_operation(
    state: &AppState,
    record: AccountRecord,
    job: Value,
) -> Result<()> {
    if let Ok(mut view) = view(state, record.id) {
        view.reset_outcome = None;
        state
            .accounts
            .inner
            .lock()
            .expect("accounts lock")
            .views
            .insert(record.id, view);
    }
    let generation = Uuid::new_v4();
    state
        .accounts
        .inner
        .lock()
        .expect("accounts lock")
        .generations
        .insert(record.id, generation);
    bridge(state, record, generation, job, None, None).await
}
pub(super) async fn bridge(
    state: &AppState,
    record: AccountRecord,
    generation: Uuid,
    mut job: Value,
    cancel: Option<oneshot::Receiver<()>>,
    mut ready: Option<oneshot::Sender<()>>,
) -> Result<()> {
    let id = record.id;
    let result = async {
        let (_, reference) = pin(state, &format!("account:{id}"))?;
        let spec = account_spec(
            &state.state_dir,
            &record.provider,
            &reference,
            reference.config_dir.clone().into(),
        );
        job["provider"] = serde_json::to_value(record.provider).map_err(ApiError::internal)?;
        job["config_dir"] = json!(reference.config_dir);
        job["config_env"] = json!(reference.config_env);
        job["program"] = json!(spec.program);
        let script = crate::bridge::script(&state.state_dir, "TENDLY_ACCOUNT_BRIDGE", "account.mjs");
        // --use-env-proxy makes the bridge's own fetch (the usage query) honour
        // the proxy; the same variables reach the native CLIs it spawns.
        let node_options: &[&str] = if record.proxy_url.is_some() { &["--use-env-proxy"] } else { &[] };
        let mut command = crate::bridge::node_command(node_options, script);
        command.current_dir(&reference.config_dir);
        for key in spec.env_remove { command.env_remove(key); } command.envs(spec.env);
        if let Some(proxy) = record.proxy_url.as_deref() {
            command.envs(proxy_environment(proxy));
        } else {
            // An account with no proxy configured stays direct rather than
            // silently inheriting whatever the host shell happened to export.
            for (key, _) in proxy_environment("") { command.env_remove(key); }
        }
        let mut child = command.spawn().map_err(|_| ApiError::bad("Account management needs Node.js and the installed official native client bridge"))?;
        let mut group = OwnedGroup::new(child.id());
        let mut input = child.stdin.take().ok_or_else(|| ApiError::bad("Account bridge input unavailable"))?;
        let io_result = async {
            tokio::time::timeout(Duration::from_secs(3), input.write_all(format!("{}\n",job).as_bytes())).await.map_err(|_|ApiError::bad("Account bridge input timed out"))?.map_err(ApiError::internal)?;
            let output = child.stdout.take().ok_or_else(|| ApiError::bad("Account bridge output unavailable"))?;
            let mut lines = BufReader::new(output.take(1024 * 1024)).lines();
            let mut cancel = cancel;
            let mut deadline = tokio::time::Instant::now() + Duration::from_secs(if job["action"] == "login" {31*60} else {60});
            let mut received_view = false;
            loop {
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => return Err(ApiError::bad("Official account operation timed out")),
                    _ = async {if let Some(receiver)=cancel.as_mut(){let _=receiver.await;}else{std::future::pending::<()>().await;}} => {
                        cancel=None;
                        tokio::time::timeout(Duration::from_secs(2),input.write_all(b"{\"type\":\"cancel\"}\n")).await.map_err(|_|ApiError::bad("Login cancellation input timed out"))?.map_err(ApiError::internal)?;
                        deadline=tokio::time::Instant::now()+Duration::from_secs(40);
                    },
                    line=lines.next_line()=> {
                        let Some(line)=line.map_err(ApiError::internal)? else {break;};
                        let event:Value=serde_json::from_str(&line).map_err(|_|ApiError::bad("Invalid account bridge response"))?;
                        let is_view=event["type"]=="view";
                        let is_login=event["type"]=="login";
                        let reset_ack=if event["type"]=="reset_authorized" {Some(json!({"type":"reset_authorized_ack","idempotency_key":event["idempotency_key"]}).to_string())}else{None};
                        // Persist retry authorization before acknowledging it. The
                        // bridge cannot consume a credit until this durable write succeeds.
                        apply_event(state,&record,generation,event)?;
                        if let Some(ack)=reset_ack {
                            tokio::time::timeout(Duration::from_secs(2),input.write_all(format!("{ack}\n").as_bytes())).await.map_err(|_|ApiError::bad("Reset authorization acknowledgement timed out"))?.map_err(ApiError::internal)?;
                        }
                        received_view|=is_view;
                        if (is_view||is_login) && let Some(ready)=ready.take(){let _=ready.send(());}
                    }
                }
            }
            let status=tokio::time::timeout(Duration::from_secs(3),child.wait()).await.map_err(|_|ApiError::bad("Native account process did not exit"))?.map_err(ApiError::internal)?;
            if !received_view||!status.success(){return Err(ApiError::bad("Native account operation did not complete"));}
            Ok(())
        }.await;
        if io_result.is_err() { let _=tokio::time::timeout(Duration::from_millis(300),input.write_all(b"{\"type\":\"cancel\"}\n")).await; }
        close_group(&mut group, &mut child).await;
        protect_files(Path::new(&reference.config_dir))?;
        io_result
    }.await;
    if let Err(ref error) = result
        && let Ok(mut current) = view(state, id)
    {
        current.status = "error".into();
        current.login = None;
        current.error = Some(error.message.clone());
        current.checked_at = Some(chrono::Utc::now().to_rfc3339());
        let mut inner = state.accounts.inner.lock().expect("accounts lock");
        if inner.generations.get(&id) == Some(&generation) {
            inner.views.insert(id, current);
        }
    }
    if let Some(ready) = ready.take() {
        let _ = ready.send(());
    }
    result
}
pub(super) fn apply_event(
    state: &AppState,
    record: &AccountRecord,
    generation: Uuid,
    event: Value,
) -> Result<()> {
    let mut current = view(state, record.id)?;
    match event["type"].as_str() {
        Some("login") => {
            let login: LoginView = serde_json::from_value(event["login"].clone())
                .map_err(|_| ApiError::bad("Invalid official login response"))?;
            let url = url::Url::parse(&login.url)
                .map_err(|_| ApiError::bad("Invalid official login URL"))?;
            if !is_official_login_url(&url) {
                return Err(ApiError::bad(
                    "Login URL is not an official HTTPS authorization URL",
                ));
            }
            current.status = "login_pending".into();
            current.login = Some(login);
            current.error = None;
        }
        Some("view") => {
            let view: BridgeView = serde_json::from_value(event["view"].clone())
                .map_err(|_| ApiError::bad("Invalid official account state"))?;
            if !matches!(
                view.status.as_str(),
                "unknown" | "signed_out" | "signed_in" | "error"
            ) {
                return Err(ApiError::bad("Invalid account status"));
            }
            current.status = view.status;
            current.email = view.email;
            current.plan = view.plan;
            current.checked_at = view.checked_at;
            current.error = view.error;
            current.limits = view.limits;
            current.capabilities = view.capabilities;
            current.usage = view.usage;
            current.login = None;
        }
        Some("reset_authorized") => {
            let key = event["idempotency_key"]
                .as_str()
                .filter(|key| {
                    !key.is_empty() && key.len() <= 200 && !key.chars().any(char::is_control)
                })
                .ok_or_else(|| ApiError::bad("Invalid reset authorization"))?;
            if state
                .accounts
                .inner
                .lock()
                .expect("accounts lock")
                .generations
                .get(&record.id)
                != Some(&generation)
            {
                return Err(ApiError::conflict(
                    "Quota reset attempt is no longer active",
                ));
            }
            persist_reset_key(state, record.id, key)?;
        }
        Some("reset") => {
            let outcome = event["outcome"]
                .as_str()
                .filter(|outcome| {
                    matches!(
                        *outcome,
                        "reset" | "alreadyRedeemed" | "nothingToReset" | "noCredit"
                    )
                })
                .ok_or_else(|| ApiError::bad("Invalid official reset result"))?;
            current.reset_outcome = Some(outcome.into());
        }
        _ => return Err(ApiError::bad("Unsupported account bridge event")),
    }
    let mut inner = state.accounts.inner.lock().expect("accounts lock");
    if inner.generations.get(&record.id) == Some(&generation) {
        inner.views.insert(record.id, current);
    }
    Ok(())
}

/// Whether a login URL the bridge relays may be shown to the user: HTTPS, no
/// embedded credentials, and on the provider's own domain. The page opens it
/// for the user to sign in, so anything else could send them elsewhere.
pub(super) fn is_official_login_url(url: &url::Url) -> bool {
    let host = url.host_str().unwrap_or("");
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && ["openai.com", "chatgpt.com"]
            .iter()
            .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
}
