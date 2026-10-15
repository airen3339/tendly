-- Query configuration and last result belong to the endpoint, never a session snapshot.
CREATE TABLE IF NOT EXISTS endpoint_balances (
    profile_id TEXT PRIMARY KEY REFERENCES endpoint_profiles(id) ON DELETE CASCADE,
    document TEXT NOT NULL
);
