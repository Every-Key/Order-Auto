CREATE TABLE stores (
  id TEXT PRIMARY KEY,
  display_name TEXT NOT NULL,
  shop_domain TEXT NOT NULL UNIQUE,
  access_token TEXT NOT NULL,
  last_connection_status TEXT,
  last_connection_message TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE batch_jobs (
  id TEXT PRIMARY KEY,
  store_id TEXT REFERENCES stores(id) ON DELETE SET NULL,
  order_template_json TEXT NOT NULL,
  requested_count INTEGER NOT NULL CHECK (requested_count BETWEEN 1 AND 100),
  status TEXT NOT NULL CHECK (
    status IN (
      'pending',
      'running',
      'stopping',
      'completed',
      'completed_with_errors',
      'paused'
    )
  ),
  created_at TEXT NOT NULL,
  started_at TEXT,
  finished_at TEXT
);

CREATE TABLE batch_items (
  id TEXT PRIMARY KEY,
  batch_id TEXT NOT NULL REFERENCES batch_jobs(id) ON DELETE CASCADE,
  sequence_number INTEGER NOT NULL CHECK (sequence_number >= 1),
  source_identifier TEXT NOT NULL UNIQUE,
  status TEXT NOT NULL CHECK (
    status IN ('queued', 'creating', 'succeeded', 'failed', 'uncertain', 'stopped')
  ),
  attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
  shopify_order_id TEXT,
  shopify_order_name TEXT,
  error_code TEXT,
  error_message TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE(batch_id, sequence_number)
);

CREATE INDEX idx_stores_display_name ON stores(display_name);
CREATE INDEX idx_stores_shop_domain ON stores(shop_domain);
CREATE INDEX idx_batch_jobs_history ON batch_jobs(store_id, created_at DESC);
CREATE INDEX idx_batch_items_batch_status ON batch_items(batch_id, status);
CREATE INDEX idx_batch_items_status ON batch_items(status);
