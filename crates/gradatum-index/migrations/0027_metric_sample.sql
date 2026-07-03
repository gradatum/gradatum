-- 0027_metric_sample.sql — timeseries de métriques curées (v0.7.5 Slice 2a F-85).
-- Format long : une ligne par (série curée, tick). Additive, jamais modifiée.
CREATE TABLE IF NOT EXISTS metric_sample (
  series  TEXT    NOT NULL,
  ts_ms   INTEGER NOT NULL,
  value   REAL    NOT NULL,
  PRIMARY KEY (series, ts_ms)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS idx_metric_sample_ts ON metric_sample(ts_ms);
