CREATE TABLE plugin_verification_anchors (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    chain_hash TEXT NOT NULL,
    signature  TEXT NOT NULL,
    public_key TEXT NOT NULL,
    at         INTEGER NOT NULL
);
