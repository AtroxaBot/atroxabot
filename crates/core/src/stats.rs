//! Usage statistics: what was used, by day, provider, model, agent and
//! tool. Every response of a model is a [`Record`], logged as it ends to a
//! file of its own (`usage.jsonl`, a line each), so what a session used
//! stays counted after the session is deleted. A [`Ledger`] is that file
//! read back, summarised for whatever period the user looks at. The
//! accounts' plan limits, as last reported, are kept beside it.
//!
//! Days are UTC days since the Unix epoch.
// ponytail: UTC days, not the user's; a reply late in the evening west of
// Greenwich lands on the next day. Use the local offset once std has one.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{Config, write_private};
use crate::engine::Limits;
use crate::error::Result;
use crate::session::{Block, Entry, Session};

/// Seconds in a day.
const DAY: u64 = 86_400;

/// The file every response is logged to, in the data folder.
const LOG_FILE: &str = "usage.jsonl";
/// The file the accounts' plan limits are kept in.
const LIMITS_FILE: &str = "limits.json";
/// Most lines of the log that are read: far more than years of use write.
const MOST_RECORDS: usize = 2_000_000;

/// One response of a model, as the statistics see it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Record {
    /// The day it was written.
    pub day: u64,
    /// The agent's id.
    pub agent: String,
    /// The session's id.
    pub session: String,
    /// The provider it came through (a connection's id); empty when not recorded.
    pub provider: String,
    /// What that provider was called then, for when it no longer exists.
    pub provider_name: String,
    /// The model that wrote it.
    pub model: String,
    /// Prompt tokens, including those read from or written to the cache.
    pub input: u64,
    /// Generated tokens.
    pub output: u64,
    /// Prompt tokens read from the cache.
    pub cached: u64,
    /// Prompt tokens written to the cache.
    pub written: u64,
    /// Its cost in USD, when known.
    pub cost: f64,
    /// The user messages it answered (the first reply of a turn carries them).
    pub prompts: u64,
    /// The tools it called, and whether each failed.
    pub tools: Vec<(String, bool)>,
}

/// Sums over some records.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Tally {
    /// Prompt tokens, cached ones included.
    pub input: u64,
    /// Generated tokens.
    pub output: u64,
    /// Prompt tokens read from the cache.
    pub cached: u64,
    /// Prompt tokens written to the cache.
    pub written: u64,
    /// Cost in USD.
    pub cost: f64,
    /// Model responses.
    pub replies: u64,
    /// User messages.
    pub prompts: u64,
    /// Tool calls.
    pub tools: u64,
}

impl Tally {
    fn add(&mut self, record: &Record) {
        self.input += record.input;
        self.output += record.output;
        self.cached += record.cached;
        self.written += record.written;
        self.cost += record.cost;
        self.replies += 1;
        self.prompts += record.prompts;
        self.tools += record.tools.len() as u64;
    }

    /// Tokens in and out.
    #[must_use]
    pub fn tokens(&self) -> u64 {
        self.input + self.output
    }

    /// The share of prompt tokens that came from the cache, `0..=1`.
    #[must_use]
    pub fn cache_rate(&self) -> f64 {
        if self.input == 0 { 0.0 } else { self.cached as f64 / self.input as f64 }
    }
}

/// A tool and how it fared.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolStat {
    /// Its name.
    pub name: String,
    /// Times it was called.
    pub calls: u64,
    /// Calls that failed or were refused.
    pub errors: u64,
}

/// The statistics of a period.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Summary {
    /// Everything in it.
    pub total: Tally,
    /// By provider (a connection's id, and what it was last called), the most tokens first.
    pub providers: Vec<(String, String, Tally)>,
    /// By model, the most tokens first.
    pub models: Vec<(String, Tally)>,
    /// By agent id, the most tokens first.
    pub agents: Vec<(String, Tally)>,
    /// By tool, the most calls first.
    pub tools: Vec<ToolStat>,
    /// Sessions something happened in.
    pub sessions: usize,
    /// Days something happened on.
    pub active_days: usize,
    /// The day with the most tokens, and how many.
    pub busiest: Option<(u64, u64)>,
}

/// Every response of every session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Ledger {
    /// The responses, in no particular order.
    pub records: Vec<Record>,
}

impl Ledger {
    /// Reads the responses out of `sessions`. A reply saved before replies
    /// were dated counts on the day its session was last changed.
    #[must_use]
    pub fn collect(sessions: &[Session]) -> Self {
        let mut records = Vec::new();
        for session in sessions {
            let mut prompts = 0;
            for entry in &session.entries {
                match entry {
                    Entry::User { .. } => prompts += 1,
                    Entry::Summary { .. } | Entry::Note { .. } => {}
                    Entry::Reply(reply) => {
                        let at = if reply.at == 0 { session.updated } else { reply.at };
                        let mut tools = Vec::new();
                        called(&reply.blocks, &mut tools);
                        records.push(Record {
                            day: at / DAY,
                            agent: session.agent.clone(),
                            session: session.id.clone(),
                            // Only a Claude account is on record in a session.
                            provider: session.account.clone(),
                            provider_name: String::new(),
                            model: if reply.model.is_empty() { "unknown".to_owned() } else { reply.model.clone() },
                            input: reply.usage.input_tokens,
                            output: reply.usage.output_tokens,
                            cached: reply.usage.cached_tokens,
                            written: reply.usage.cache_write_tokens,
                            cost: reply.cost,
                            prompts: std::mem::take(&mut prompts),
                            tools,
                        });
                    }
                }
            }
        }
        Self { records }
    }

    /// Where responses are logged: `usage.jsonl` in the data folder.
    ///
    /// # Errors
    /// [`crate::Error::NoHomeDir`] if the platform reports no home directory.
    pub fn path() -> Result<PathBuf> {
        Ok(Config::dir()?.join(LOG_FILE))
    }

    /// Reads the log at `path`: a record a line, lines that are not one
    /// skipped. `None` when there is no log yet.
    #[must_use]
    pub fn read(path: &Path) -> Option<Self> {
        let text = match fs::read(path) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(e) if e.kind() == ErrorKind::NotFound => return None,
            // A log that cannot be read is no reason to count everything twice.
            Err(_) => return Some(Self::default()),
        };
        let records = text.lines().take(MOST_RECORDS).filter_map(|line| serde_json::from_str(line).ok()).collect();
        Some(Self { records })
    }

    /// Writes the whole log to `path`, replacing what is there.
    ///
    /// # Errors
    /// Any I/O failure.
    pub fn write(&self, path: &Path) -> Result<()> {
        let mut text = Vec::new();
        for record in &self.records {
            serde_json::to_writer(&mut text, record)?;
            text.push(b'\n');
        }
        write_private(path, &text)
    }

    /// Adds `record` to the log at `path`, as a line at its end.
    ///
    /// # Errors
    /// Any I/O failure.
    pub fn append(path: &Path, record: &Record) -> Result<()> {
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        let mut options = fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        // One write, so a line is never half there beside another's.
        Ok(options.open(path)?.write_all(&line)?)
    }

    /// What each day used, days without anything left out.
    #[must_use]
    pub fn days(&self) -> BTreeMap<u64, Tally> {
        let mut days: BTreeMap<u64, Tally> = BTreeMap::new();
        for record in &self.records {
            days.entry(record.day).or_default().add(record);
        }
        days
    }

    /// The statistics of the days from `from` on.
    #[must_use]
    pub fn summary(&self, from: u64) -> Summary {
        let mut summary = Summary::default();
        let (mut models, mut agents) = (BTreeMap::<&str, Tally>::new(), BTreeMap::<&str, Tally>::new());
        let mut providers = BTreeMap::<&str, (&str, Tally)>::new();
        let mut tools = BTreeMap::<&str, ToolStat>::new();
        let (mut sessions, mut days) = (HashSet::new(), BTreeMap::<u64, u64>::new());
        for record in self.records.iter().filter(|record| record.day >= from) {
            summary.total.add(record);
            let provider = providers.entry(&record.provider).or_default();
            if !record.provider_name.is_empty() {
                provider.0 = &record.provider_name;
            }
            provider.1.add(record);
            models.entry(&record.model).or_default().add(record);
            agents.entry(&record.agent).or_default().add(record);
            for (name, failed) in &record.tools {
                let tool = tools.entry(name).or_default();
                tool.calls += 1;
                tool.errors += u64::from(*failed);
            }
            sessions.insert(&record.session);
            *days.entry(record.day).or_default() += record.input + record.output;
        }
        let ranked = |groups: BTreeMap<&str, Tally>| {
            let mut groups: Vec<(String, Tally)> =
                groups.into_iter().map(|(key, tally)| (key.to_owned(), tally)).collect();
            groups.sort_by_key(|(_, tally)| std::cmp::Reverse(tally.tokens()));
            groups
        };
        summary.providers =
            providers.into_iter().map(|(id, (name, tally))| (id.to_owned(), name.to_owned(), tally)).collect();
        summary.providers.sort_by_key(|(_, _, tally)| std::cmp::Reverse(tally.tokens()));
        summary.models = ranked(models);
        summary.agents = ranked(agents);
        summary.tools = tools.into_iter().map(|(name, tool)| ToolStat { name: name.to_owned(), ..tool }).collect();
        summary.tools.sort_by_key(|tool| std::cmp::Reverse(tool.calls));
        summary.sessions = sessions.len();
        summary.active_days = days.len();
        summary.busiest = days.into_iter().max_by_key(|(_, tokens)| *tokens);
        summary
    }

    /// How many days in a row something happened, up to `today` (or
    /// yesterday, if today is still quiet), and the longest such run ever.
    #[must_use]
    pub fn streaks(&self, today: u64) -> (u64, u64) {
        let days: Vec<u64> = self.days().into_keys().collect();
        let (mut longest, mut run, mut previous) = (0, 0, None);
        for day in &days {
            run = if previous.is_some_and(|previous| previous + 1 == *day) { run + 1 } else { 1 };
            longest = longest.max(run);
            previous = Some(*day);
        }
        // The run that ends today, or yesterday while today is still open.
        let current = match previous {
            Some(last) if last == today || last + 1 == today => run,
            _ => 0,
        };
        (current, longest)
    }
}

/// The accounts' plan limits as they were last reported, by connection id.
/// Nothing when none were kept, or the file cannot be read.
#[must_use]
pub fn load_limits() -> HashMap<String, Limits> {
    let read = || serde_json::from_slice(&fs::read(Config::dir().ok()?.join(LIMITS_FILE)).ok()?).ok();
    read().unwrap_or_default()
}

/// Keeps the accounts' plan limits for the next start.
///
/// # Errors
/// No home directory, or any I/O failure.
pub fn save_limits<S: std::hash::BuildHasher>(limits: &HashMap<String, Limits, S>) -> Result<()> {
    write_private(&Config::dir()?.join(LIMITS_FILE), &serde_json::to_vec(limits)?)
}

/// The tools called in `blocks` and whether each failed, those of subagents included.
fn called(blocks: &[Block], out: &mut Vec<(String, bool)>) {
    for block in blocks {
        if let Block::Tool(call) = block {
            out.push((call.name.clone(), call.is_error));
            called(&call.sub, out);
        }
    }
}

/// The day (since the Unix epoch) of a time in seconds.
#[must_use]
pub fn day_of(seconds: u64) -> u64 {
    seconds / DAY
}

/// The calendar date of a day since the Unix epoch: year, month (1 to 12)
/// and day of the month.
#[must_use]
pub fn civil(day: u64) -> (u64, u32, u32) {
    // Howard Hinnant's `civil_from_days`, for days on or after the epoch.
    let z = day + 719_468;
    let era = z / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day_of_month = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 { shifted_month + 3 } else { shifted_month - 9 } as u32;
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    (year, month, day_of_month)
}

/// The weekday of a day since the Unix epoch: 0 for Sunday to 6 for Saturday.
#[must_use]
pub fn weekday(day: u64) -> u32 {
    // The epoch was a Thursday.
    ((day + 4) % 7) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::responses::Usage;
    use crate::session::{Reply, ToolUse};

    fn reply(at: u64, model: &str, usage: Usage, cost: f64, tools: &[(&str, bool)]) -> Entry {
        let blocks = tools
            .iter()
            .map(|(name, failed)| {
                Block::Tool(ToolUse { name: (*name).into(), is_error: *failed, ..ToolUse::default() })
            })
            .collect();
        Entry::Reply(Reply { blocks, model: model.into(), usage, cost, at, ..Reply::default() })
    }

    fn user() -> Entry {
        Entry::User { text: "hi".into(), note: String::new(), files: Vec::new() }
    }

    #[test]
    fn sessions_are_tallied_by_day_model_agent_and_tool() {
        let usage = |input, output, cached| Usage {
            input_tokens: input,
            output_tokens: output,
            cached_tokens: cached,
            cache_write_tokens: 0,
        };
        let mut a = Session::new("scout");
        a.entries = vec![
            user(),
            reply(10 * DAY + 5, "opus", usage(100, 10, 80), 0.5, &[("Read", false), ("Bash", true)]),
            reply(10 * DAY + 9, "opus", usage(200, 20, 150), 0.25, &[("Read", false)]),
            user(),
            reply(11 * DAY, "sonnet", usage(50, 5, 0), 0.0, &[]),
        ];
        let mut b = Session::new("press");
        b.updated = 13 * DAY + 1;
        // Saved before replies were dated: counted on the session's last day.
        b.entries = vec![user(), reply(0, "", usage(1_000, 100, 0), 1.0, &[])];
        let ledger = Ledger::collect(&[a, b]);
        assert_eq!(ledger.records.len(), 4);
        assert_eq!(ledger.records.iter().map(|r| r.prompts).collect::<Vec<_>>(), [1, 0, 1, 1]);

        let all = ledger.summary(0);
        assert_eq!((all.total.input, all.total.output, all.total.replies, all.total.prompts), (1_350, 135, 4, 3));
        assert_eq!((all.total.tools, all.sessions, all.active_days), (3, 2, 3));
        assert!((all.total.cost - 1.75).abs() < 1e-9);
        assert_eq!(all.models.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(), ["unknown", "opus", "sonnet"]);
        assert_eq!(all.agents[0].0, "press");
        assert_eq!(all.tools[0], ToolStat { name: "Read".into(), calls: 2, errors: 0 });
        assert_eq!(all.tools[1], ToolStat { name: "Bash".into(), calls: 1, errors: 1 });
        assert_eq!(all.busiest, Some((13, 1_100)));
        assert!((all.models[1].1.cache_rate() - 230.0 / 300.0).abs() < 1e-9);

        // A period only counts its own days.
        let late = ledger.summary(11);
        assert_eq!((late.total.replies, late.sessions, late.models.len()), (2, 2, 2));
        assert_eq!(ledger.days().keys().copied().collect::<Vec<_>>(), [10, 11, 13]);
        // Two days in a row, then a gap: the run that counts ended on day 13.
        assert_eq!(ledger.streaks(13), (1, 2));
        assert_eq!(ledger.streaks(14), (1, 2), "yesterday still counts while today is quiet");
        assert_eq!(ledger.streaks(16), (0, 2));
        assert_eq!(Ledger::default().streaks(5), (0, 0));
    }

    #[test]
    fn the_log_outlives_the_sessions() {
        let dir = std::env::temp_dir().join(format!("atroxabot-usage-{}", crate::new_id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("usage.jsonl");
        assert!(Ledger::read(&path).is_none(), "no log yet");
        let record = |provider: &str, name: &str, input: u64| Record {
            day: 10,
            provider: provider.into(),
            provider_name: name.into(),
            model: "opus".into(),
            input,
            tools: vec![("Read".into(), false)],
            ..Record::default()
        };
        Ledger { records: vec![record("work", "Work", 100)] }.write(&path).expect("write");
        Ledger::append(&path, &record("work", "Office", 50)).expect("append");
        Ledger::append(&path, &record("", "", 7)).expect("append");
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .and_then(|mut f| f.write_all(b"not a record\n{\"day\":\n"))
            .expect("junk");
        let ledger = Ledger::read(&path).expect("a log");
        assert_eq!(ledger.records.len(), 3, "lines that are no records are skipped");
        let summary = ledger.summary(0);
        // A provider is known by its id, under the name it was last given.
        let providers: Vec<(&str, &str, u64)> =
            summary.providers.iter().map(|(id, name, t)| (id.as_str(), name.as_str(), t.input)).collect();
        assert_eq!(providers, [("work", "Office", 150), ("", "", 7)]);
        assert_eq!(summary.total.tools, 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn days_become_dates() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(19_723), (2024, 1, 1));
        assert_eq!(civil(19_782), (2024, 2, 29), "a leap day");
        assert_eq!(civil(20_767), (2026, 11, 10));
        assert_eq!((weekday(0), weekday(3), weekday(20_767)), (4, 0, 2), "Thursday, Sunday, Tuesday");
        assert_eq!(day_of(20_767 * DAY + 5), 20_767);
    }
}
