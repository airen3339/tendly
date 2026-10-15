//! A workspace's saved pane layout.

use super::*;

pub(super) async fn get_layout(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
) -> Result<Json<Value>> {
    workspace(&state, id).await?;
    let layout = db(&state, move |s| s.get_layout(id))
        .await?
        .map(|s| serde_json::from_str::<Value>(&s))
        .transpose()
        .map_err(ApiError::internal)?;
    // Legacy pre-ID M0 layouts cannot be rendered; return a valid default.
    Ok(Json(
        layout.filter(valid_layout).unwrap_or_else(default_layout),
    ))
}
pub(super) async fn save_layout(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
    Json(layout): Json<Value>,
) -> Result<StatusCode> {
    workspace(&state, id).await?;
    if !valid_layout(&layout) || layout.to_string().len() > 256 * 1024 {
        return Err(ApiError::bad("Invalid or oversized layout"));
    }
    db(&state, move |s| s.save_layout(id, &layout.to_string())).await?;
    Ok(StatusCode::NO_CONTENT)
}
pub(super) fn default_layout() -> Value {
    json!({"version":1,"root":{"type":"split","id":"root","direction":"horizontal","ratio":0.5,"first":{"type":"pane","id":"agent","kind":"agent_chat","title":"Agent"},"second":{"type":"pane","id":"changes","kind":"git_diff","title":"Changes"}}})
}
pub(super) fn valid_layout(layout: &Value) -> bool {
    fn node(value: &Value, seen: &mut HashSet<String>, depth: usize, count: &mut usize) -> bool {
        *count += 1;
        if depth > 32 || *count > 512 {
            return false;
        }
        let Some(id) = value
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && s.len() < 200)
        else {
            return false;
        };
        if !seen.insert(id.to_owned()) {
            return false;
        }
        match value.get("type").and_then(Value::as_str) {
            Some("pane") => matches!(
                value.get("kind").and_then(Value::as_str),
                Some("agent_chat" | "terminal" | "editor" | "file_preview" | "git_diff")
            ),
            Some("split") => {
                value
                    .get("ratio")
                    .and_then(Value::as_f64)
                    .is_some_and(|n| (0.03..=0.97).contains(&n))
                    && matches!(
                        value.get("direction").and_then(Value::as_str),
                        Some("horizontal" | "vertical")
                    )
                    && value
                        .get("first")
                        .is_some_and(|n| node(n, seen, depth + 1, count))
                    && value
                        .get("second")
                        .is_some_and(|n| node(n, seen, depth + 1, count))
            }
            Some("stack") => value
                .get("panes")
                .and_then(Value::as_array)
                .is_some_and(|panes| {
                    panes.iter().all(|p| {
                        p.get("type") == Some(&json!("pane")) && node(p, seen, depth + 1, count)
                    })
                }),
            _ => false,
        }
    }
    layout.get("version") == Some(&json!(1))
        && layout
            .get("root")
            .is_some_and(|r| node(r, &mut HashSet::new(), 0, &mut 0))
}

/// The routes this module serves.
pub(super) fn routes() -> Router<AppState> {
    Router::new().route(
        "/api/workspaces/{id}/layout",
        get(get_layout).put(save_layout),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(id: &str, kind: &str) -> Value {
        json!({"type": "pane", "id": id, "kind": kind})
    }

    #[test]
    fn the_default_layout_is_valid() {
        assert!(valid_layout(&default_layout()));
    }

    #[test]
    fn a_layout_is_refused_for_bad_ratios_kinds_ids_or_shape() {
        let split = |ratio: f64, first: Value, second: Value| json!({"version": 1, "root": {"type": "split", "id": "s", "direction": "vertical", "ratio": ratio, "first": first, "second": second}});
        assert!(valid_layout(&split(
            0.5,
            pane("a", "terminal"),
            pane("b", "editor")
        )));
        assert!(
            !valid_layout(&split(0.02, pane("a", "terminal"), pane("b", "editor"))),
            "too narrow"
        );
        assert!(
            !valid_layout(&split(0.5, pane("a", "terminal"), pane("a", "editor"))),
            "duplicate id"
        );
        assert!(
            !valid_layout(&split(0.5, pane("a", "browser"), pane("b", "editor"))),
            "unknown kind"
        );
        assert!(
            !valid_layout(&split(0.5, pane("", "terminal"), pane("b", "editor"))),
            "empty id"
        );
        assert!(
            !valid_layout(&json!({"version": 2, "root": pane("a", "terminal")})),
            "unknown version"
        );
        let stack = json!({"version": 1, "root": {"type": "stack", "id": "t", "panes": [pane("a", "terminal"), {"type": "split", "id": "x"}]}});
        assert!(!valid_layout(&stack), "a stack holds panes only");
    }

    #[test]
    fn a_layout_cannot_nest_without_bound() {
        let mut node = pane("leaf", "terminal");
        for depth in 0..40 {
            node = json!({"type": "split", "id": format!("s{depth}"), "direction": "horizontal", "ratio": 0.5, "first": node, "second": pane(&format!("p{depth}"), "editor")});
        }
        assert!(!valid_layout(&json!({"version": 1, "root": node})));
    }
}
