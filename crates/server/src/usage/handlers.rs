//! The /api/usage routes: the report, the user's prices, and where the
//! transcripts to read are.

use super::*;

#[derive(Deserialize)]
pub struct UsageQuery {
    /// Unix seconds; defaults to thirty days ago.
    from: Option<i64>,
    to: Option<i64>,
    /// Minutes east of UTC, for local days and hours.
    tz: Option<i32>,
    provider: Option<String>,
    model: Option<String>,
    project: Option<String>,
}

/// The user's own homes, every home a session was given, every account and
/// every directory a session was pointed at; and the sessions to name calls by.
fn homes(state: &AppState) -> Result<(Vec<Home>, HashMap<String, SessionLink>)> {
    let sessions = state
        .store
        .list_sessions(None)
        .map_err(crate::ApiError::internal)?;
    let mut homes: Vec<Home> = state
        .native_sources
        .iter()
        .map(|source| (source.config_dir.clone(), None))
        .collect();
    for root in ["sessions", "accounts"] {
        if let Ok(entries) = std::fs::read_dir(state.state_dir.join(root)) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let session = (root == "sessions").then(|| Arc::from(name.as_str()));
                homes.push((entry.path(), session));
            }
        }
    }
    let mut links = HashMap::new();
    for session in &sessions {
        if let Some(dir) = &session.native_config_dir {
            homes.push((PathBuf::from(dir), None));
        }
        let link = || SessionLink {
            id: session.id.to_string(),
            title: session.title.clone(),
        };
        links.insert(session.id.to_string(), link());
        if let Some(native) = &session.provider_session_id {
            links.insert(native.clone(), link());
        }
    }
    Ok((homes, links))
}

/// Read every transcript once, in the background after start, so the usage
/// page opens on figures instead of on a first full read of months of logs.
pub fn warm(state: AppState) {
    tokio::task::spawn_blocking(move || {
        if let Ok((homes, _)) = homes(&state) {
            collect(&homes);
        }
    });
}

async fn usage(
    State(state): State<AppState>,
    Query(query): Query<UsageQuery>,
) -> Result<Json<Report>> {
    let now = Utc::now().timestamp();
    let to = query.to.unwrap_or(now).min(now + 86_400);
    let from = query.from.unwrap_or(to - 30 * 86_400).min(to);
    let tz = query.tz.unwrap_or(0);
    let filter = Filter {
        provider: query
            .provider
            .as_deref()
            .and_then(ProviderKind::parse_agent),
        model: query.model.filter(|model| !model.is_empty()),
        project: query.project.filter(|project| !project.is_empty()),
    };
    let (homes, links) = homes(&state)?;
    let prices = load_prices(&state.state_dir);
    let report = tokio::task::spawn_blocking(move || {
        let calls = collect(&homes);
        report(&calls, from, to, tz, &links, &prices, &filter)
    })
    .await
    .map_err(crate::ApiError::internal)?;
    Ok(Json(report))
}

async fn get_prices(State(state): State<AppState>) -> Json<Prices> {
    Json(load_prices(&state.state_dir))
}

/// Replace the user's prices. Kept small and sane: a typo must not become a
/// cost a thousand times too large without the page saying so.
async fn put_prices(
    State(state): State<AppState>,
    Json(prices): Json<Prices>,
) -> Result<Json<Prices>> {
    if prices.models.len() > 200 {
        return Err(crate::ApiError::bad("Too many model prices"));
    }
    let mut clean = Prices::default();
    for (model, price) in prices.models {
        let model = model.trim().to_ascii_lowercase();
        let sane = |value: f64| value.is_finite() && (0.0..=10_000.0).contains(&value);
        if model.is_empty()
            || model.len() > 200
            || ![price.input, price.output, price.cache_read]
                .into_iter()
                .chain(price.cache_write)
                .chain(price.cache_write_1h)
                .all(sane)
        {
            return Err(crate::ApiError::bad("Invalid model price"));
        }
        clean.models.insert(model, price);
    }
    let bytes = serde_json::to_vec_pretty(&clean).map_err(crate::ApiError::internal)?;
    std::fs::write(prices_path(&state.state_dir), bytes).map_err(crate::ApiError::internal)?;
    Ok(Json(clean))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/usage", get(usage))
        .route("/api/usage/prices", get(get_prices).put(put_prices))
        .route("/api/usage/allowance", get(allowance))
}
