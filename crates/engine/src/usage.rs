//! Device-local token usage ledger and summary watches.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

use chrono::{Local, TimeZone as _, Utc};
use rusqlite::{Connection, OptionalExtension as _, params};
use tokio::sync::watch;

use jolt_proto::{
    AgentEvent, CostProvenance, HarnessId, UsageBreakdown, UsageBreakdownRow, UsageDay,
    UsageSummary,
};

use crate::pricing::PricingCatalog;

#[derive(Debug, Clone)]
pub(crate) struct UsageContext {
    pub harness: HarnessId,
    pub model: String,
    pub cwd: String,
    pub service_tier: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UsagePurpose {
    Chat,
    TitleGeneration,
    QuestionExtraction,
}

impl UsagePurpose {
    fn key(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::TitleGeneration => "title-generation",
            Self::QuestionExtraction => "question-extraction",
        }
    }
}

pub(crate) struct UsageCapture {
    store: UsageStore,
    chat_id: String,
    purpose: UsagePurpose,
    context: UsageContext,
}

impl UsageCapture {
    pub(crate) fn new(
        store: UsageStore,
        chat_id: &str,
        purpose: UsagePurpose,
        harness: HarnessId,
        model: Option<&str>,
        cwd: &str,
    ) -> Self {
        Self {
            store,
            chat_id: chat_id.to_string(),
            purpose,
            context: UsageContext {
                harness,
                model: model.unwrap_or_default().to_string(),
                cwd: cwd.to_string(),
                service_tier: None,
            },
        }
    }

    pub(crate) fn observe(&mut self, event: &AgentEvent) -> rusqlite::Result<()> {
        match event {
            AgentEvent::SessionStarted {
                harness,
                model,
                cwd,
                ..
            } => {
                self.context = UsageContext {
                    harness: *harness,
                    model: model.clone(),
                    cwd: cwd.clone(),
                    service_tier: None,
                };
                Ok(())
            }
            AgentEvent::Usage { .. } => {
                self.store
                    .record_internal(&self.chat_id, self.purpose, &self.context, event)
            }
            _ => Ok(()),
        }
    }
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
    cost_provenance: Option<CostProvenance>,
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
    cost_provenance: Option<CostProvenance>,
}

#[derive(Clone)]
pub struct UsageStore {
    device_id: String,
    connection: std::sync::Arc<Mutex<Connection>>,
    watches: std::sync::Arc<Mutex<HashMap<String, watch::Sender<UsageSummary>>>>,
    pricing: PricingCatalog,
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

fn merge_cost_provenance(total: &mut Option<CostProvenance>, value: Option<CostProvenance>) {
    *total = match (*total, value) {
        (Some(CostProvenance::Mixed), _) | (_, Some(CostProvenance::Mixed)) => {
            Some(CostProvenance::Mixed)
        }
        (Some(left), Some(right)) if left != right => Some(CostProvenance::Mixed),
        (left, right) => left.or(right),
    };
}

fn provenance_key(provenance: CostProvenance) -> &'static str {
    match provenance {
        CostProvenance::ProviderReported => "provider-reported",
        CostProvenance::ModelEstimated => "model-estimated",
        CostProvenance::Mixed => "mixed",
    }
}

fn parse_provenance(value: Option<&str>) -> Option<CostProvenance> {
    match value {
        Some("provider-reported") => Some(CostProvenance::ProviderReported),
        Some("model-estimated") => Some(CostProvenance::ModelEstimated),
        Some("mixed") => Some(CostProvenance::Mixed),
        _ => None,
    }
}

fn initialize_schema(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch(
        "PRAGMA journal_mode=WAL;
         CREATE TABLE IF NOT EXISTS usage_events (
             chat_id TEXT NOT NULL,
             journal_seq INTEGER NOT NULL,
             device_id TEXT NOT NULL,
             harness TEXT NOT NULL,
             model TEXT NOT NULL,
             cwd TEXT NOT NULL,
             purpose TEXT NOT NULL DEFAULT 'chat',
             recorded_at_ms INTEGER NOT NULL,
             input_tokens INTEGER NOT NULL,
             output_tokens INTEGER NOT NULL,
             cache_read_input_tokens INTEGER NOT NULL,
             cache_write_input_tokens INTEGER NOT NULL,
             cost_usd REAL,
             cost_provenance TEXT,
             context_tokens INTEGER,
             context_window INTEGER,
             PRIMARY KEY (chat_id, journal_seq)
         );
         CREATE INDEX IF NOT EXISTS usage_events_recorded_at
             ON usage_events(recorded_at_ms);",
    )?;
    let has_cost_provenance: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM pragma_table_info('usage_events')
            WHERE name = 'cost_provenance'
         )",
        [],
        |row| row.get(0),
    )?;
    if !has_cost_provenance {
        connection.execute(
            "ALTER TABLE usage_events ADD COLUMN cost_provenance TEXT",
            [],
        )?;
    }
    Ok(())
}

pub(crate) fn ensure_schema(path: &Path) -> rusqlite::Result<()> {
    initialize_schema(&Connection::open(path)?)
}

impl UsageStore {
    fn insert_usage_event(
        &self,
        connection: &Connection,
        chat_id: &str,
        journal_seq: i64,
        purpose: UsagePurpose,
        context: &UsageContext,
        event: &AgentEvent,
    ) -> rusqlite::Result<()> {
        let AgentEvent::Usage {
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_write_input_tokens,
            cost_usd,
            cost_provenance,
            context_tokens,
            context_window,
        } = event
        else {
            return Ok(());
        };
        let reported_cost = (*cost_usd).filter(|cost| cost.is_finite() && *cost >= 0.0);
        let (cost_usd, cost_provenance) = if let Some(cost) = reported_cost {
            let provenance = (*cost_provenance).or(Some(match context.harness {
                HarnessId::Pi => CostProvenance::ModelEstimated,
                _ => CostProvenance::ProviderReported,
            }));
            (Some(cost), provenance)
        } else if context.harness == HarnessId::Codex {
            (
                self.pricing.estimate_codex(
                    &context.model,
                    *input_tokens,
                    *output_tokens,
                    *cache_read_input_tokens,
                    *cache_write_input_tokens,
                    context.service_tier.as_deref(),
                ),
                Some(CostProvenance::ModelEstimated),
            )
        } else {
            (None, None)
        };
        let cost_provenance = cost_usd.and(cost_provenance).map(provenance_key);
        connection.execute(
            "INSERT OR IGNORE INTO usage_events (
            chat_id, journal_seq, device_id, harness, model, cwd, purpose,
            recorded_at_ms, input_tokens, output_tokens,
            cache_read_input_tokens, cache_write_input_tokens, cost_usd,
            cost_provenance, context_tokens, context_window
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                chat_id,
                journal_seq,
                self.device_id,
                harness_key(context.harness),
                context.model,
                context.cwd,
                purpose.key(),
                Utc::now().timestamp_millis(),
                sql_u64(*input_tokens),
                sql_u64(*output_tokens),
                sql_u64(*cache_read_input_tokens),
                sql_u64(*cache_write_input_tokens),
                cost_usd,
                cost_provenance,
                context_tokens.map(sql_u64),
                context_window.map(sql_u64),
            ],
        )?;
        Ok(())
    }

    pub fn open(path: &Path, device_id: String) -> rusqlite::Result<Self> {
        let pricing_dir = path.parent().unwrap_or_else(|| Path::new("."));
        Self::open_with_pricing(path, device_id, PricingCatalog::load(pricing_dir))
    }

    pub(crate) fn open_with_pricing(
        path: &Path,
        device_id: String,
        pricing: PricingCatalog,
    ) -> rusqlite::Result<Self> {
        let connection = Connection::open(path)?;
        initialize_schema(&connection)?;
        Ok(Self {
            device_id,
            connection: std::sync::Arc::new(Mutex::new(connection)),
            watches: std::sync::Arc::new(Mutex::new(HashMap::new())),
            pricing,
        })
    }

    pub(crate) fn record(
        &self,
        chat_id: &str,
        journal_seq: u64,
        context: &UsageContext,
        event: &AgentEvent,
    ) -> rusqlite::Result<()> {
        {
            let connection = lock(&self.connection);
            self.insert_usage_event(
                &connection,
                chat_id,
                sql_u64(journal_seq),
                UsagePurpose::Chat,
                context,
                event,
            )?;
        }
        self.refresh_summary(chat_id)
    }

    fn record_internal(
        &self,
        chat_id: &str,
        purpose: UsagePurpose,
        context: &UsageContext,
        event: &AgentEvent,
    ) -> rusqlite::Result<()> {
        if !matches!(event, AgentEvent::Usage { .. }) {
            return Ok(());
        }
        {
            let connection = lock(&self.connection);
            let journal_seq = connection.query_row(
                "SELECT COALESCE(MIN(journal_seq), 0) - 1
                 FROM usage_events WHERE chat_id = ?1",
                [chat_id],
                |row| row.get(0),
            )?;
            self.insert_usage_event(&connection, chat_id, journal_seq, purpose, context, event)?;
        }
        self.refresh_summary(chat_id)
    }

    fn refresh_summary(&self, chat_id: &str) -> rusqlite::Result<()> {
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
        {
            let mut statement = connection.prepare(
                "SELECT DISTINCT cost_provenance FROM usage_events
                 WHERE chat_id = ?1 AND cost_usd IS NOT NULL
                       AND cost_provenance IS NOT NULL",
            )?;
            let provenances = statement.query_map([chat_id], |row| row.get::<_, String>(0))?;
            for provenance in provenances {
                merge_cost_provenance(
                    &mut summary.cost_provenance,
                    parse_provenance(Some(&provenance?)),
                );
            }
        }
        if let Some((harness, model)) = connection
            .query_row(
                "SELECT harness, model FROM usage_events
                 WHERE chat_id = ?1 AND purpose = 'chat'
                 ORDER BY recorded_at_ms DESC, journal_seq DESC LIMIT 1",
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
                 WHERE chat_id = ?1 AND purpose = 'chat' AND context_tokens IS NOT NULL
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
                    cache_write_input_tokens, cost_usd, cost_provenance
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
                    cost_provenance: parse_provenance(row.get::<_, Option<String>>(10)?.as_deref()),
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
            merge_cost_provenance(&mut result.cost_provenance, record.cost_provenance);

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
            merge_cost_provenance(&mut day_row.cost_provenance, record.cost_provenance);

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
            merge_cost_provenance(&mut group.cost_provenance, record.cost_provenance);
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
                cost_provenance: group.cost_provenance,
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
            cost_provenance: Some(CostProvenance::ModelEstimated),
            context_tokens: Some(context),
            context_window: Some(200_000),
        }
    }

    #[test]
    fn adds_cost_provenance_to_existing_usage_ledgers() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("usage.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE usage_events (
                    chat_id TEXT NOT NULL, journal_seq INTEGER NOT NULL,
                    device_id TEXT NOT NULL, harness TEXT NOT NULL,
                    model TEXT NOT NULL, cwd TEXT NOT NULL,
                    purpose TEXT NOT NULL, recorded_at_ms INTEGER NOT NULL,
                    input_tokens INTEGER NOT NULL, output_tokens INTEGER NOT NULL,
                    cache_read_input_tokens INTEGER NOT NULL,
                    cache_write_input_tokens INTEGER NOT NULL, cost_usd REAL,
                    context_tokens INTEGER, context_window INTEGER,
                    PRIMARY KEY (chat_id, journal_seq)
                 );",
            )
            .unwrap();
        drop(connection);

        let store = UsageStore::open(&path, "d1".into()).unwrap();
        let has_column: bool = lock(&store.connection)
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM pragma_table_info('usage_events')
                    WHERE name = 'cost_provenance'
                 )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(has_column);
    }

    #[test]
    fn records_cumulative_summary_and_breakdown() {
        let dir = tempdir().unwrap();
        let store = UsageStore::open(&dir.path().join("usage.sqlite"), "d1".into()).unwrap();
        let context = UsageContext {
            harness: HarnessId::Pi,
            model: "anthropic/sonnet".into(),
            cwd: "/repo".into(),
            service_tier: None,
        };
        store.record("c1", 1, &context, &usage(10, 20, 30)).unwrap();
        store.record("c1", 2, &context, &usage(11, 21, 32)).unwrap();

        let summary = store.summary("c1").unwrap();
        assert_eq!(summary.input_tokens, 21);
        assert_eq!(summary.cache_read_input_tokens, 41);
        assert_eq!(summary.context_tokens, Some(32));
        assert_eq!(summary.cost_usd, Some(0.5));
        assert_eq!(
            summary.cost_provenance,
            Some(CostProvenance::ModelEstimated)
        );

        let breakdown = store.breakdown(30).unwrap();
        assert_eq!(breakdown.sessions, 1);
        assert_eq!(breakdown.calls, 2);
        assert_eq!(breakdown.rows[0].total_tokens(), 76);
    }

    #[test]
    fn codex_cost_is_estimated_once_and_never_repriced() {
        let dir = tempdir().unwrap();
        let cache = dir.path().join("model-prices.json");
        let write_rate = |input_rate: f64| {
            std::fs::write(
                &cache,
                format!(
                    r#"{{"gpt-test":{{"litellm_provider":"openai","input_cost_per_token":{input_rate},"output_cost_per_token":0}}}}"#
                ),
            )
            .unwrap();
        };
        write_rate(1.0);
        let path = dir.path().join("usage.sqlite");
        let context = UsageContext {
            harness: HarnessId::Codex,
            model: "gpt-test".into(),
            cwd: "/repo".into(),
            service_tier: None,
        };
        let event = AgentEvent::Usage {
            input_tokens: 2,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            cost_usd: None,
            cost_provenance: None,
            context_tokens: Some(2),
            context_window: Some(200_000),
        };
        let store = UsageStore::open(&path, "d1".into()).unwrap();
        store.record("c1", 1, &context, &event).unwrap();
        assert_eq!(store.summary("c1").unwrap().cost_usd, Some(2.0));
        drop(store);

        write_rate(10.0);
        let store = UsageStore::open(&path, "d1".into()).unwrap();
        let historical = store.summary("c1").unwrap();
        assert_eq!(historical.cost_usd, Some(2.0));
        assert_eq!(
            historical.cost_provenance,
            Some(CostProvenance::ModelEstimated)
        );
        store.record("c1", 2, &context, &event).unwrap();
        assert_eq!(store.summary("c1").unwrap().cost_usd, Some(22.0));
    }

    #[test]
    fn codex_fast_uses_api_priority_rates() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("model-prices.json"),
            r#"{"gpt-test":{"litellm_provider":"openai","input_cost_per_token":1,"output_cost_per_token":1,"input_cost_per_token_priority":2,"output_cost_per_token_priority":2}}"#,
        )
        .unwrap();
        let store = UsageStore::open(&dir.path().join("usage.sqlite"), "d1".into()).unwrap();
        let context = UsageContext {
            harness: HarnessId::Codex,
            model: "gpt-test".into(),
            cwd: "/repo".into(),
            service_tier: Some("fast".into()),
        };
        let event = AgentEvent::Usage {
            input_tokens: 1,
            output_tokens: 1,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            cost_usd: None,
            cost_provenance: None,
            context_tokens: None,
            context_window: None,
        };
        store.record("c1", 1, &context, &event).unwrap();
        assert_eq!(store.summary("c1").unwrap().cost_usd, Some(4.0));
    }

    #[test]
    fn provider_reported_cost_precedes_codex_estimate() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("model-prices.json"),
            r#"{"gpt-test":{"litellm_provider":"openai","input_cost_per_token":10,"output_cost_per_token":10}}"#,
        )
        .unwrap();
        let store = UsageStore::open(&dir.path().join("usage.sqlite"), "d1".into()).unwrap();
        let context = UsageContext {
            harness: HarnessId::Codex,
            model: "gpt-test".into(),
            cwd: "/repo".into(),
            service_tier: None,
        };
        let event = AgentEvent::Usage {
            input_tokens: 1,
            output_tokens: 1,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            cost_usd: Some(0.5),
            cost_provenance: Some(CostProvenance::ProviderReported),
            context_tokens: None,
            context_window: None,
        };
        store.record("c1", 1, &context, &event).unwrap();
        let summary = store.summary("c1").unwrap();
        assert_eq!(summary.cost_usd, Some(0.5));
        assert_eq!(
            summary.cost_provenance,
            Some(CostProvenance::ProviderReported)
        );
    }

    #[test]
    fn missing_exact_codex_rate_stays_unpriced() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("model-prices.json"),
            r#"{"gpt-test":{"litellm_provider":"openai","input_cost_per_token":1,"output_cost_per_token":1}}"#,
        )
        .unwrap();
        let store = UsageStore::open(&dir.path().join("usage.sqlite"), "d1".into()).unwrap();
        let context = UsageContext {
            harness: HarnessId::Codex,
            model: "openai/gpt-test".into(),
            cwd: "/repo".into(),
            service_tier: None,
        };
        let event = AgentEvent::Usage {
            input_tokens: 1,
            output_tokens: 1,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            cost_usd: None,
            cost_provenance: None,
            context_tokens: None,
            context_window: None,
        };
        store.record("c1", 1, &context, &event).unwrap();
        let summary = store.summary("c1").unwrap();
        assert_eq!(summary.cost_usd, None);
        assert_eq!(summary.cost_provenance, None);
    }

    #[test]
    fn internal_usage_counts_without_replacing_active_chat_context() {
        let dir = tempdir().unwrap();
        let store = UsageStore::open(&dir.path().join("usage.sqlite"), "d1".into()).unwrap();
        let chat_context = UsageContext {
            harness: HarnessId::ClaudeCode,
            model: "sonnet".into(),
            cwd: "/repo".into(),
            service_tier: None,
        };
        store
            .record("c1", 1, &chat_context, &usage(100, 20, 120))
            .unwrap();

        let mut capture = UsageCapture::new(
            store.clone(),
            "c1",
            UsagePurpose::TitleGeneration,
            HarnessId::ClaudeCode,
            Some("haiku"),
            "/repo",
        );
        capture
            .observe(&AgentEvent::SessionStarted {
                harness: HarnessId::ClaudeCode,
                model: "haiku".into(),
                tools: Vec::new(),
                cwd: "/repo".into(),
                session_id: "title-run".into(),
                assistant_message_id: "assistant".into(),
            })
            .unwrap();
        capture.observe(&usage(10, 2, 12)).unwrap();

        let summary = store.summary("c1").unwrap();
        assert_eq!(summary.calls, 2);
        assert_eq!(summary.input_tokens, 110);
        assert_eq!(summary.model.as_deref(), Some("sonnet"));
        assert_eq!(summary.context_tokens, Some(120));

        let connection = lock(&store.connection);
        let internal = connection
            .query_row(
                "SELECT purpose, journal_seq FROM usage_events WHERE purpose != 'chat'",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .unwrap();
        assert_eq!(internal.0, "title-generation");
        assert!(internal.1 <= 0);
        drop(connection);

        let breakdown = store.breakdown(30).unwrap();
        assert_eq!(breakdown.calls, 2);
        assert_eq!(breakdown.rows.len(), 2);
    }
}
