//! Token usage, read from the clients' own logs.
//!
//! Claude Code and Codex each write every model call, with its token counts,
//! into JSONL transcripts under their config directory. Reading those -- the
//! user's own, every isolated session home, every account -- counts everything
//! the host spent, including sessions started outside Tendly, and needs no
//! cooperation from the clients.
//!
//! Files are parsed once and then only from where they were last read, since a
//! transcript only ever grows; a request after the first costs the new lines.
use crate::{AppState, Result};
use axum::{
    Json, Router,
    extract::{Query, State},
    routing::get,
};
use chrono::{DateTime, Datelike, Duration as ChronoDuration, FixedOffset, Timelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::File,
    io::{BufRead, BufReader, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::SystemTime,
};
use tendly_domain::ProviderKind;

mod allowance;
mod handlers;
mod prices;
mod report;
#[cfg(test)]
mod tests;
mod transcripts;

pub use allowance::*;
pub use handlers::*;
pub use prices::*;
pub use report::*;
pub(crate) use transcripts::*;
