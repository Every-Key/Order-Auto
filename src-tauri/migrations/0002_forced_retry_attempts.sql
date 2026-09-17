CREATE TABLE forced_retry_attempts (
  item_id TEXT NOT NULL REFERENCES batch_items(id) ON DELETE CASCADE,
  attempt_number INTEGER NOT NULL CHECK (attempt_number >= 1),
  risk_confirmed INTEGER NOT NULL CHECK (risk_confirmed = 1),
  confirmed_at TEXT NOT NULL,
  PRIMARY KEY (item_id, attempt_number)
);
