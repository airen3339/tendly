//! Moving state over from AgentDock, Tendly's name until 0.1.75.
//!
//! The first start finds `~/.agentdock` and no `~/.tendly`, and moves it: the
//! directory, the files named after the program, and the `AGENTDOCK_` names
//! stored inside (endpoint secrets and the profiles that reference them). It
//! runs once per machine and only when nothing is in the way, so it can stay
//! until every install it was written for has started once, and then go.

use std::{
    fs,
    path::{Path, PathBuf},
};

const OLD_DIR: &str = ".agentdock";
const OLD_PREFIX: &str = "AGENTDOCK_";
const NEW_PREFIX: &str = "TENDLY_";

/// Files that carry the old name, and what they are called now.
const RENAMED: &[(&str, &str)] = &[
    ("agentdock.db", "tendly.db"),
    ("agentdock.db-wal", "tendly.db-wal"),
    ("agentdock.db-shm", "tendly.db-shm"),
    ("agentdock-metrics.db", "tendly-metrics.db"),
    ("agentdock-metrics.db-wal", "tendly-metrics.db-wal"),
    ("agentdock-metrics.db-shm", "tendly-metrics.db-shm"),
    ("agentdock.log", "tendly.log"),
];
/// Left by a gateway that is no longer running; the new one makes its own.
const DROPPED: &[&str] = &["agentdock.pid", "agentdock.db.agentdock.lock"];

/// Move `<home>/.agentdock` to `state` when only the former exists.
///
/// Refuses while an AgentDock gateway still runs from it: moving its database
/// from under it would lose whatever it writes next.
pub fn from_agentdock(home: &Path, state: &Path) -> Result<Option<PathBuf>, String> {
    let old = home.join(OLD_DIR);
    if state.exists() || !old.is_dir() {
        return Ok(None);
    }
    // Every AgentDock server holds this lock while it runs, however it was
    // started; a pid file exists only for the background mode, so it missed a
    // gateway run by systemd. Held through the move, so none can start midway.
    let _running = hold_server_lock(&old).map_err(|_| {
        format!(
            "AgentDock is still running from {}. Stop it first (`agentdock stop`, or the service that runs it, e.g. `sudo systemctl stop agentdock`), then start Tendly again; its state moves to {} on that start.",
            old.display(),
            state.display()
        )
    })?;
    fs::rename(&old, state).map_err(|error| {
        format!(
            "Could not move {} to {}: {error}",
            old.display(),
            state.display()
        )
    })?;
    for (from, to) in RENAMED {
        let from = state.join(from);
        if from.exists() {
            fs::rename(&from, state.join(to))
                .map_err(|error| format!("Could not rename {}: {error}", from.display()))?;
        }
    }
    for name in DROPPED {
        let _ = fs::remove_file(state.join(name));
    }
    for text in ["secrets.toml", "config.toml"] {
        let path = state.join(text);
        if let Ok(contents) = fs::read_to_string(&path)
            && contents.contains(OLD_PREFIX)
        {
            fs::write(&path, contents.replace(OLD_PREFIX, NEW_PREFIX))
                .map_err(|error| format!("Could not rewrite {}: {error}", path.display()))?;
        }
    }
    let database = state.join("tendly.db");
    if database.is_file() {
        rename_in_database(&database)
            .map_err(|error| format!("Could not update {}: {error}", database.display()))?;
    }
    Ok(Some(old))
}

/// The lock an AgentDock server holds on its state directory, or an error when
/// one is holding it now.
fn hold_server_lock(old: &Path) -> std::io::Result<Option<fs::File>> {
    let path = old.join("server.lock");
    if !path.is_file() {
        return Ok(None);
    }
    let file = fs::OpenOptions::new().read(true).write(true).open(path)?;
    file.try_lock().map_err(|error| match error {
        fs::TryLockError::WouldBlock => std::io::Error::from(std::io::ErrorKind::WouldBlock),
        fs::TryLockError::Error(error) => error,
    })?;
    Ok(Some(file))
}

/// Every text value naming an `AGENTDOCK_` variable, renamed: a profile's
/// `env:AGENTDOCK_SECRET_…` key and any stored environment override.
fn rename_in_database(path: &Path) -> rusqlite::Result<()> {
    let connection = rusqlite::Connection::open(path)?;
    let tables: Vec<String> = connection
        .prepare(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        )?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let quote = |name: &str| format!("\"{}\"", name.replace('"', "\"\""));
    for table in tables {
        let columns: Vec<String> = connection
            .prepare(&format!(
                "SELECT name FROM pragma_table_info({})",
                quote(&table).replace('"', "'")
            ))?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for column in columns {
            let column = quote(&column);
            connection.execute(
                &format!(
                    "UPDATE {table} SET {column} = replace({column}, ?1, ?2) \
                     WHERE typeof({column}) = 'text' AND instr({column}, ?1) > 0",
                    table = quote(&table)
                ),
                [OLD_PREFIX, NEW_PREFIX],
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agentdock_state_moves_over_with_its_names() {
        let home = std::env::temp_dir().join(format!("tendly-rename-{}", uuid::Uuid::new_v4()));
        let old = home.join(OLD_DIR);
        fs::create_dir_all(old.join("sessions")).unwrap();
        fs::write(old.join("sessions/kept"), "x").unwrap();
        fs::write(old.join("token"), "secret-token").unwrap();
        fs::write(old.join("agentdock.log"), "log").unwrap();
        fs::write(old.join("agentdock.pid"), "999999999").unwrap();
        fs::write(old.join("secrets.toml"), "AGENTDOCK_SECRET_WORK = \"sk\"\n").unwrap();
        {
            let db = rusqlite::Connection::open(old.join("agentdock.db")).unwrap();
            db.execute_batch(
                "CREATE TABLE profiles (id INTEGER, api_key TEXT, n INTEGER);
                 INSERT INTO profiles VALUES (1, 'env:AGENTDOCK_SECRET_WORK', 7);
                 INSERT INTO profiles VALUES (2, 'plain', 8);",
            )
            .unwrap();
        }
        let state = home.join(".tendly");

        assert_eq!(from_agentdock(&home, &state).unwrap(), Some(old.clone()));

        assert!(!old.exists());
        assert_eq!(
            fs::read_to_string(state.join("sessions/kept")).unwrap(),
            "x"
        );
        assert_eq!(
            fs::read_to_string(state.join("token")).unwrap(),
            "secret-token"
        );
        assert!(state.join("tendly.log").is_file());
        assert!(!state.join("agentdock.pid").exists());
        assert_eq!(
            fs::read_to_string(state.join("secrets.toml")).unwrap(),
            "TENDLY_SECRET_WORK = \"sk\"\n"
        );
        let db = rusqlite::Connection::open(state.join("tendly.db")).unwrap();
        let keys: Vec<String> = db
            .prepare("SELECT api_key FROM profiles ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(keys, ["env:TENDLY_SECRET_WORK", "plain"]);
        // Windows will not delete a database that is still open.
        drop(db);

        // Once moved there is nothing left to do.
        assert_eq!(from_agentdock(&home, &state).unwrap(), None);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn existing_tendly_state_is_left_alone() {
        let home = std::env::temp_dir().join(format!("tendly-rename-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(home.join(OLD_DIR)).unwrap();
        fs::create_dir_all(home.join(".tendly")).unwrap();
        assert_eq!(from_agentdock(&home, &home.join(".tendly")).unwrap(), None);
        assert!(home.join(OLD_DIR).is_dir());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_running_agentdock_is_not_moved_from_under() {
        let home = std::env::temp_dir().join(format!("tendly-rename-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(home.join(OLD_DIR)).unwrap();
        // What a running AgentDock holds, whether started in the background or
        // by a service manager with no pid file.
        let lock = fs::File::create(home.join(OLD_DIR).join("server.lock")).unwrap();
        lock.try_lock().unwrap();
        let error = from_agentdock(&home, &home.join(".tendly")).unwrap_err();
        assert!(error.contains("systemctl stop agentdock"), "{error}");
        assert!(home.join(OLD_DIR).is_dir());
        lock.unlock().unwrap();
        drop(lock);
        assert!(
            from_agentdock(&home, &home.join(".tendly"))
                .unwrap()
                .is_some()
        );
        fs::remove_dir_all(home).unwrap();
    }
}
