//! Device-local token usage ledger and summary watches.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

use chrono::{Local, TimeZone as _, Utc};
use rusqlite::{Connection, OptionalExtension as _, params};
use tokio::sync::watch;

use jolt_proto::{
    AgentEvent, HarnessId, UsageBreakdown, UsageBreakdownRow, UsageDay, UsageSummary,
};

#[derive(Debug, Clone)]
pub(crate) struct UsageContext {
    pub harness: HarnessId,
    pub model: String,
    pub cwd: String,
}

#[derive(Debug)]
struct RawUsage {
    chat_id: String,
    harness: HarnessId,
    model: String,
    cwd: String,
    recorded_at_ms: i64,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_input_tokens: u64,
    cache_write_input_tokens: u64,
    cost_usd: Option<f64>,
}

#[derive(Default)]
struct GroupAccumulator {
    chats: HashSet<String>,
    calls: u64,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_input_tokens: u64,
    cache_write_input_tokens: u64,
    cost_usd: Option<f64>,
}

#[derive(Clone)]
pub struct UsageStore {
    device_id: String,
    connection: std::sync::Arc<Mutex<Connection>>,
    watches: std::sync::Arc<Mutex<HashMap<String, watch::Sender<UsageSummary>>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn harness_key(harness: HarnessId) -> &'static str {
    match harness {
        HarnessId::ClaudeCode => "claude-code",
        HarnessId::Codex => "codex",
        HarnessId::Pi => "pi",
        HarnessId::Mock => "mock",
    }
}

fn parse_harness(value: &str) -> HarnessId {
    match value {
        "claude-code" => HarnessId::ClaudeCode,
        "codex" => HarnessId::Codex,
        "pi" => HarnessId::Pi,
        _ => HarnessId::Mock,
    }
}

fn sql_u64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn add_cost(total: &mut Option<f64>, value: Option<f64>) {
    if let Some(value) = value {
        *total = Some(total.unwrap_or_default() + value);
    }
}

impl UsageStore {
    pub fn open(path: &Path, device_id: String) -> rusqlite::Result<Self> {
        let connection = Connection::open(path)?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS usage_events (
                 chat_id TEXT NOT NULL,
                 journal_seq INTEGER NOT NULL,
                 device_id TEXT NOT NULL,
                 harness TEXT NOT NULL,
                 model TEXT NOT NULL,
                 cwd TEXT NOT NULL,
                 recorded_at_ms INTEGER NOT NULL,
                 input_tokens INTEGER NOT NULL,
                 output_tokens INTEGER NOT NULL,
                 cache_read_input_tokens INTEGER NOT NULL,
                 cache_write_input_tokens INTEGER NOT NULL,
                 cost_usd REAL,
                 context_tokens INTEGER,
                 context_window INTEGER,
                 PRIMARY KEY (chat_id, journal_seq)
             );
             CREATE INDEX IF NOT EXISTS usage_events_recorded_at
                 ON usage_events(recorded_at_ms);",
        )?;
        Ok(Self {
            device_id,
            connection: std::sync::Arc::new(Mutex::new(connection)),
            watches: std::sync::Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub(crate) fn record(
        &self,
        chat_id: &str,
        journal_seq: u64,
        context: &UsageContext,
        event: &AgentEvent,
    ) -> rusqlite::Result<()> {
        let AgentEvent::Usage {
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_write_input_tokens,
            cost_usd,
            context_tokens,
            context_window,
        } = event
        else {
            return Ok(());
        };
        let recorded_at_ms = Utc::now().timestamp_millis();
        {
            let connection = lock(&self.connection);
            connection.execute(
                "INSERT OR IGNORE INTO usage_events (
                    chat_id, journal_seq, device_id, harness, model, cwd,
                    recorded_at_ms, input_tokens, output_tokens,
                    cache_read_input_tokens, cache_write_input_tokens, cost_usd,
                    context_tokens, context_window
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    chat_id,
                    sql_u64(journal_seq),
                    self.device_id,
                    harness_key(context.harness),
                    context.model,
                    context.cwd,
                    recorded_at_ms,
                    sql_u64(*input_tokens),
                    sql_u64(*output_tokens),
                    sql_u64(*cache_read_input_tokens),
                    sql_u64(*cache_write_input_tokens),
                    cost_usd,
                    context_tokens.map(sql_u64),
                    context_window.map(sql_u64),
                ],
            )?;
        }
        let summary = self.summary(chat_id)?;
        if let Some(tx) = lock(&self.watches).get(chat_id) {
            tx.send_replace(summary);
        }
        Ok(())
    }

    pub fn watch_chat(&self, chat_id: &str) -> rusqlite::Result<watch::Receiver<UsageSummary>> {
        let summary = self.summary(chat_id)?;
        let mut watches = lock(&self.watches);
        Ok(match watches.get(chat_id) {
            Some(tx) => tx.subscribe(),
            None => {
                let (tx, rx) = watch::channel(summary);
                watches.insert(chat_id.to_string(), tx);
                rx
            }
        })
    }

    pub fn summary(&self, chat_id: &str) -> rusqlite::Result<UsageSummary> {
        let connection = lock(&self.connection);
        let mut summary = connection.query_row(
            "SELECT COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(cache_read_input_tokens), 0),
                    COALESCE(SUM(cache_write_input_tokens), 0),
                    COUNT(*), SUM(cost_usd), MAX(recorded_at_ms)
             FROM usage_events WHERE chat_id = ?1",
            [chat_id],
            |row| {
                Ok(UsageSummary {
                    chat_id: chat_id.to_string(),
                    input_tokens: row.get::<_, i64>(0)?.max(0) as u64,
                    output_tokens: row.get::<_, i64>(1)?.max(0) as u64,
                    cache_read_input_tokens: row.get::<_, i64>(2)?.max(0) as u64,
                    cache_write_input_tokens: row.get::<_, i64>(3)?.max(0) as u64,
                    calls: row.get::<_, i64>(4)?.max(0) as u64,
                    cost_usd: row.get(5)?,
                    updated_at_ms: row.get(6)?,
                    ..UsageSummary::default()
                })
            },
        )?;
        if let Some((harness, model)) = connection
            .query_row(
                "SELECT harness, model FROM usage_events
                 WHERE chat_id = ?1 ORDER BY recorded_at_ms DESC, journal_seq DESC LIMIT 1",
                [chat_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
        {
            summary.harness = Some(parse_harness(&harness));
            summary.model = Some(model);
        }
        if let Some((context_tokens, context_window)) = connection
            .query_row(
                "SELECT context_tokens, context_window FROM usage_events
                 WHERE chat_id = ?1 AND context_tokens IS NOT NULL
                 ORDER BY recorded_at_ms DESC, journal_seq DESC LIMIT 1",
                [chat_id],
                |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, Option<i64>>(1)?)),
            )
            .optional()?
        {
            summary.context_tokens = context_tokens.map(|value| value.max(0) as u64);
            summary.context_window = context_window.map(|value| value.max(0) as u64);
        }
        Ok(summary)
    }

    pub fn breakdown(&self, days: u16) -> rusqlite::Result<UsageBreakdown> {
        let cutoff = Utc::now()
            .timestamp_millis()
            .saturating_sub(i64::from(days) * 86_400_000);
        let connection = lock(&self.connection);
        let mut statement = connection.prepare(
            "SELECT chat_id, harness, model, cwd, recorded_at_ms,
                    input_tokens, output_tokens, cache_read_input_tokens,
                    cache_write_input_tokens, cost_usd
             FROM usage_events WHERE recorded_at_ms >= ?1
             ORDER BY recorded_at_ms ASC",
        )?;
        let records = statement
            .query_map([cutoff], |row| {
                Ok(RawUsage {
                    chat_id: row.get(0)?,
                    harness: parse_harness(&row.get::<_, String>(1)?),
                    model: row.get(2)?,
                    cwd: row.get(3)?,
                    recorded_at_ms: row.get(4)?,
                    input_tokens: row.get::<_, i64>(5)?.max(0) as u64,
                    output_tokens: row.get::<_, i64>(6)?.max(0) as u64,
                    cache_read_input_tokens: row.get::<_, i64>(7)?.max(0) as u64,
                    cache_write_input_tokens: row.get::<_, i64>(8)?.max(0) as u64,
                    cost_usd: row.get(9)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        drop(connection);

        let mut chats = HashSet::new();
        let mut activity: BTreeMap<String, UsageDay> = BTreeMap::new();
        let mut groups: HashMap<(HarnessId, String, String), GroupAccumulator> = HashMap::new();
        let mut result = UsageBreakdown {
            device_id: self.device_id.clone(),
            days,
            ..UsageBreakdown::default()
        };
        for record in records {
            chats.insert(record.chat_id.clone());
            result.calls = result.calls.saturating_add(1);
            result.input_tokens = result.input_tokens.saturating_add(record.input_tokens);
            result.output_tokens = result.output_tokens.saturating_add(record.output_tokens);
            result.cache_read_input_tokens = result
                .cache_read_input_tokens
                .saturating_add(record.cache_read_input_tokens);
            result.cache_write_input_tokens = result
                .cache_write_input_tokens
                .saturating_add(record.cache_write_input_tokens);
            add_cost(&mut result.cost_usd, record.cost_usd);

            let day = Local
                .timestamp_millis_opt(record.recorded_at_ms)
                .single()
                .map(|timestamp| timestamp.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let day_row = activity.entry(day.clone()).or_insert_with(|| UsageDay {
                day,
                ..UsageDay::default()
            });
            day_row.calls = day_row.calls.saturating_add(1);
            day_row.tokens = day_row
                .tokens
                .saturating_add(record.input_tokens)
                .saturating_add(record.output_tokens)
                .saturating_add(record.cache_read_input_tokens)
                .saturating_add(record.cache_write_input_tokens);
            add_cost(&mut day_row.cost_usd, record.cost_usd);

            let group = groups
                .entry((record.harness, record.model, record.cwd))
                .or_default();
            group.chats.insert(record.chat_id);
            group.calls = group.calls.saturating_add(1);
            group.input_tokens = group.input_tokens.saturating_add(record.input_tokens);
            group.output_tokens = group.output_tokens.saturating_add(record.output_tokens);
            group.cache_read_input_tokens = group
                .cache_read_input_tokens
                .saturating_add(record.cache_read_input_tokens);
            group.cache_write_input_tokens = group
                .cache_write_input_tokens
                .saturating_add(record.cache_write_input_tokens);
            add_cost(&mut group.cost_usd, record.cost_usd);
        }
        result.sessions = chats.len() as u64;
        result.activity = activity.into_values().collect();
        result.rows = groups
            .into_iter()
            .map(|((harness, model, cwd), group)| UsageBreakdownRow {
                harness,
                model,
                cwd,
                sessions: group.chats.len() as u64,
                calls: group.calls,
                input_tokens: group.input_tokens,
                output_tokens: group.output_tokens,
                cache_read_input_tokens: group.cache_read_input_tokens,
                cache_write_input_tokens: group.cache_write_input_tokens,
                cost_usd: group.cost_usd,
            })
            .collect();
        result
            .rows
            .sort_by_key(|row| std::cmp::Reverse(row.total_tokens()));
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn usage(input: u64, cache: u64, context: u64) -> AgentEvent {
        AgentEvent::Usage {
            input_tokens: input,
            output_tokens: 7,
            cache_read_input_tokens: cache,
            cache_write_input_tokens: 0,
            cost_usd: Some(0.25),
            context_tokens: Some(context),
            context_window: Some(200_000),
        }
    }

    #[test]
    fn records_cumulative_summary_and_breakdown() {
        let dir = tempdir().unwrap();
        let store = UsageStore::open(&dir.path().join("usage.sqlite"), "d1".into()).unwrap();
        let context = UsageContext {
            harness: HarnessId::Pi,
            model: "anthropic/sonnet".into(),
            cwd: "/repo".into(),
        };
        store.record("c1", 1, &context, &usage(10, 20, 30)).unwrap();
        store.record("c1", 2, &context, &usage(11, 21, 32)).unwrap();

        let summary = store.summary("c1").unwrap();
        assert_eq!(summary.input_tokens, 21);
        assert_eq!(summary.cache_read_input_tokens, 41);
        assert_eq!(summary.context_tokens, Some(32));
        assert_eq!(summary.cost_usd, Some(0.5));

        let breakdown = store.breakdown(30).unwrap();
        assert_eq!(breakdown.sessions, 1);
        assert_eq!(breakdown.calls, 2);
        assert_eq!(breakdown.rows[0].total_tokens(), 76);
    }
}
