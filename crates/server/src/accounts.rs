//! Account metadata plus official native-client account operations. Credential
//! contents are never parsed, copied into profiles, or returned by this module.
use crate::{ApiError, AppState, Result};
use axum::{
    Json, Router,
    extract::{Path as RoutePath, State},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tendly_domain::{EndpointProfile, NativeConfigReference, ProviderKind, SessionStatus};
use tendly_runtime::{SpawnSpec, process::OwnedGroup};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    sync::oneshot,
    task::JoinHandle,
};
use uuid::Uuid;

mod bridge;
mod handlers;
mod manager;
mod native;
mod storage;
#[cfg(test)]
mod tests;
mod types;
mod views;

use bridge::*;
pub use handlers::*;
pub use manager::*;
pub use native::*;
use storage::*;
pub use types::*;
use views::*;
