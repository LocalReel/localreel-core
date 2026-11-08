CREATE TABLE plugin_package_verifications (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    plugin_id             TEXT NOT NULL,
    plugin_version        TEXT NOT NULL,
    package_sha256        TEXT NOT NULL,
    publisher             TEXT,
    public_key_fingerprint TEXT,
    signature_verified    INTEGER NOT NULL,
    at                    INTEGER NOT NULL
);
CREATE INDEX plugin_package_verifications_plugin
    ON plugin_package_verifications(plugin_id, at DESC);
