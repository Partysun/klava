//! What this extension has saved, recorded by the extension itself.
//!
//! # Why a file, and not a counter
//!
//! klava spawns this binary once per request and it exits, so there is no
//! process left to hold a counter. One JSON line per run, appended to a state
//! file, is the smallest thing that survives that — and it keeps working when
//! klava is not running at all (`klava-compress --stats`).
//!
//! # Why not scrape the log
//!
//! klava forwards at most 20 stderr lines per run and this extension prints its
//! summary *last*, so the numbers are the first thing lost exactly when a
//! request is noisy enough to be worth measuring. The journal also rotates, and
//! nothing guarantees the same text format twice.
//!
//! # Why there are two totals
//!
//! The agent re-sends its whole conversation every turn, so the same tool result
//! is compressed again on every turn. Summing the per-run numbers counts each
//! of those again. [`Aggregate::unique_before`] and its neighbours deduplicate
//! by content hash: they answer "how many tokens would never have been sent if
//! each distinct message were compressed once". Both are reported, each
//! labelled, and neither is claimed to be a bill — the count comes from this
//! extension's tokenizer, not from the provider's.
//!
//! # Failure
//!
//! Every entry point here is best-effort. A read-only home, a full disk or a
//! concurrent writer must never turn into a failed compression, so writes are
//! capped-and-rotated rather than unbounded, and reads skip lines they cannot
//! parse.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Version of the line format below. Bump only when a reader must distinguish.
pub const VERSION: u32 = 1;

/// Basename of the state file inside the data directory.
const FILE: &str = "compress-stats.jsonl";

/// One rotated generation. Only the newest is kept: the cap exists so a year of
/// use cannot fill a disk, not to archive every request forever.
const ROTATED_SUFFIX: &str = ".1";

/// Rotate once the file passes this. A run is roughly 150 bytes, so 16 MiB is
/// about 100,000 requests — a year of heavy use — and small enough to read in
/// one pass.
const MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Per-message savings stored in one line. A request with more rewritten
/// messages than this still reports its totals; only the deduplication detail
/// for the tail is dropped, which bounds the line at a few kilobytes.
pub const MAX_SAVES: usize = 256;

/// What one message saved, keyed by its content.
///
/// The hash is of the text **before** the rewrite. Two runs over the same tool
/// output produce the same hash, which is what makes deduplication possible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageSave {
    pub hash: String,
    pub before: usize,
    pub after: usize,
}

/// One run of the extension: one request it was spawned for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    #[serde(default)]
    pub v: u32,
    /// Unix seconds when the run finished.
    #[serde(default)]
    pub ts: u64,
    /// Version of the binary that wrote the line.
    #[serde(default)]
    pub version: String,
    /// Messages rewritten, i.e. what `before`/`after` describe.
    #[serde(default)]
    pub messages: usize,
    #[serde(default)]
    pub before: usize,
    #[serde(default)]
    pub after: usize,
    /// Messages the planner chose to run, including ones an inference later
    /// rejected.
    #[serde(default)]
    pub planned: usize,
    /// Messages left alone, by reason.
    #[serde(default)]
    pub skipped: BTreeMap<String, usize>,
    /// Per-message detail for the deduplicated total.
    #[serde(default)]
    pub saved: Vec<MessageSave>,
}

impl RunRecord {
    /// An empty record, stamped with the current time and version.
    pub fn new() -> Self {
        Self {
            v: VERSION,
            ts: now(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            messages: 0,
            before: 0,
            after: 0,
            planned: 0,
            skipped: BTreeMap::new(),
            saved: Vec::new(),
        }
    }
}

impl Default for RunRecord {
    fn default() -> Self {
        Self::new()
    }
}

/// Unix seconds, or `0` if the clock is before the epoch.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `$XDG_DATA_HOME/klava`, else `$HOME/.local/share/klava`.
///
/// State, not cache: these numbers survive `--stats` runs, and a cache cleaner
/// may delete `$XDG_CACHE_HOME`. Matches where klava keeps its own state
/// (`klava-auth.json`, the installed extensions).
pub fn data_root() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_DATA_HOME")
        && !dir.trim().is_empty()
    {
        return PathBuf::from(dir).join("klava");
    }
    // No `HOME` on Windows; the same fallback the model cache uses keeps the
    // behaviour predictable rather than inventing a second convention.
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
        .join(".local")
        .join("share")
        .join("klava")
}

/// The state file.
pub fn path() -> PathBuf {
    data_root().join(FILE)
}

/// The rotated file and the state file, oldest first.
pub fn files() -> Vec<PathBuf> {
    let current = path();
    vec![rotated(&current), current]
}

fn rotated(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(ROTATED_SUFFIX);
    PathBuf::from(name)
}

/// Append one run to the state file in the default data directory.
pub fn record(run: &RunRecord) -> io::Result<()> {
    append(&path(), run, MAX_BYTES)
}

/// What reading the state files found.
#[derive(Debug, Default)]
pub struct History {
    /// Parsed runs, oldest first.
    pub runs: Vec<RunRecord>,
    /// Lines that did not parse: a killed process, or a collision.
    pub malformed: usize,
    /// Files that exist but could not be read, with the reason.
    ///
    /// Reported rather than ignored: `--stats` printing "no runs yet" over an
    /// unreadable file would be a lie about the data, not a lack of data.
    pub unreadable: Vec<(PathBuf, String)>,
}

impl History {
    /// Parse every recorded run, oldest first.
    ///
    /// A malformed line is expected, not exceptional: two concurrent runs can
    /// collide, and a killed process can leave a partial line. Counting them and
    /// reporting the rest is better than refusing to report at all.
    pub fn load(paths: &[PathBuf]) -> Self {
        let mut history = Self::default();
        for path in paths {
            let text = match fs::read_to_string(path) {
                Ok(text) => text,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => {
                    history.unreadable.push((path.clone(), e.to_string()));
                    continue;
                }
            };
            for line in text.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<RunRecord>(line) {
                    Ok(run) => history.runs.push(run),
                    Err(_) => history.malformed += 1,
                }
            }
        }
        history
    }
}

/// Append one run to `path`, rotating first when it has grown past `cap`.
fn append(path: &Path, run: &RunRecord, cap: u64) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    rotate(path, cap);

    let mut line = serde_json::to_vec(run).map_err(io::Error::other)?;
    line.push(b'\n');

    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    // klava spawns one process per request, so two runs can finish at the same
    // instant. A short write with `O_APPEND` is already atomic on Linux and
    // macOS; the lock makes that true on Windows too and costs nothing next to a
    // model load. It is *not* held indefinitely: a writer that hangs must not
    // drag this process past the host's deadline, and a reader tolerates the
    // truncated line that follows a collision.
    let locked = lock_briefly(&file);
    let written = file.write_all(&line);
    if locked {
        let _ = file.unlock();
    }
    written
}

/// Take an exclusive lock, waiting at most a few milliseconds.
///
/// Returns whether the lock is held and must be released.
fn lock_briefly(file: &std::fs::File) -> bool {
    /// Five attempts, 10 ms apart: long enough for a concurrent writer's
    /// single short write, far short of anything klava would notice.
    const ATTEMPTS: u32 = 5;
    for attempt in 0..ATTEMPTS {
        match file.try_lock() {
            Ok(()) => return true,
            // Locking is unsupported on some filesystems (an old NFS mount, a
            // FAT volume). Appending without it is still better than dropping
            // the line.
            Err(std::fs::TryLockError::Error(e)) if e.kind() == io::ErrorKind::Unsupported => {
                return false;
            }
            Err(_) if attempt + 1 == ATTEMPTS => return false,
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
        }
    }
    false
}

/// Move a file that has passed `cap` aside, keeping one generation.
fn rotate(path: &Path, cap: u64) {
    let too_big = fs::metadata(path).map(|m| m.len() >= cap).unwrap_or(false);
    if !too_big {
        return;
    }
    let old = rotated(path);
    let _ = fs::remove_file(&old);
    let _ = fs::rename(path, &old);
}

/// Content key for a message that was compressed.
///
/// 128 bits from two independent-ish FNV-1a passes. A real digest would mean a
/// new dependency in a binary that ships as one file; at the 10^5 messages this
/// file holds, the chance of a collision is about 10^-28, and a collision would
/// only under-count one message by a few hundred tokens.
pub fn hash_text(text: &str) -> String {
    const BASIS_A: u64 = 0xcbf2_9ce4_8422_2325;
    const BASIS_B: u64 = 0x8422_2325_cbf2_9ce4;
    const PRIME_A: u64 = 0x0000_0100_0000_01b3;
    const PRIME_B: u64 = 0x8803_55f2_1e6d_1965;
    format!(
        "{:016x}{:016x}",
        fnv1a(text.as_bytes(), BASIS_A, PRIME_A),
        fnv1a(text.as_bytes(), BASIS_B, PRIME_B)
    )
}

fn fnv1a(bytes: &[u8], basis: u64, prime: u64) -> u64 {
    let mut hash = basis;
    for &byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(prime);
    }
    hash
}

/// One day of runs, for the per-day table.
#[derive(Debug, Default, Clone, Serialize)]
pub struct Day {
    pub runs: usize,
    pub messages: usize,
    pub before: usize,
    pub after: usize,
}

/// Everything the state file says, totalled.
#[derive(Debug, Default, Serialize)]
pub struct Aggregate {
    /// Every run that reached the statistics point, compressed or not.
    pub runs: usize,
    /// Runs that rewrote at least one message.
    pub runs_compressed: usize,
    /// Messages rewritten, summed over runs.
    pub messages: usize,
    /// Messages the planner chose to run, summed over runs.
    pub planned: usize,
    /// Tokens in the rewritten messages, summed over runs.
    pub before: usize,
    /// Tokens after rewriting, summed over runs.
    pub after: usize,
    /// Messages left alone, by reason, summed over runs.
    pub skipped: BTreeMap<String, usize>,
    pub first_ts: Option<u64>,
    pub last_ts: Option<u64>,
    /// Per-day totals, keyed `YYYY-MM-DD` in UTC.
    pub days: BTreeMap<String, Day>,
    /// Distinct messages seen, by content hash.
    pub unique_messages: usize,
    pub unique_before: usize,
    pub unique_after: usize,
}

impl Aggregate {
    /// Total runs and totals. Duplicate content is resolved last-wins: if a
    /// threshold change made the same message compress differently, the newest
    /// result is the one being sent today.
    pub fn of(runs: &[RunRecord]) -> Self {
        let mut agg = Self::default();
        let mut unique: BTreeMap<&str, (usize, usize)> = BTreeMap::new();

        for run in runs {
            agg.runs += 1;
            if run.messages > 0 {
                agg.runs_compressed += 1;
                agg.messages += run.messages;
                agg.before += run.before;
                agg.after += run.after;
            }
            agg.planned += run.planned;
            for (reason, count) in &run.skipped {
                *agg.skipped.entry(reason.clone()).or_default() += count;
            }
            agg.first_ts = Some(agg.first_ts.map_or(run.ts, |t| t.min(run.ts)));
            agg.last_ts = Some(agg.last_ts.map_or(run.ts, |t| t.max(run.ts)));

            let day = agg.days.entry(date_of(run.ts)).or_default();
            day.runs += 1;
            day.messages += run.messages;
            day.before += run.before;
            day.after += run.after;

            for save in &run.saved {
                unique.insert(&save.hash, (save.before, save.after));
            }
        }

        agg.unique_messages = unique.len();
        agg.unique_before = unique.values().map(|(before, _)| before).sum();
        agg.unique_after = unique.values().map(|(_, after)| after).sum();
        agg
    }

    /// Tokens not sent, summed over runs.
    pub fn saved(&self) -> usize {
        self.before.saturating_sub(self.after)
    }

    /// Tokens not sent, each distinct message counted once.
    pub fn unique_saved(&self) -> usize {
        self.unique_before.saturating_sub(self.unique_after)
    }
}

/// `YYYY-MM-DD` in UTC for a Unix timestamp.
///
/// UTC, not local time, because a timezone file is not something a
/// single-binary extension should need; the header says so.
pub fn date_of(ts: u64) -> String {
    let (year, month, day) = civil_from_days((ts / 86_400) as i64);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Days since 1970-01-01 to a civil date, by Howard Hinnant's algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(ts: u64, messages: usize, before: usize, after: usize) -> RunRecord {
        RunRecord {
            ts,
            messages,
            before,
            after,
            ..RunRecord::new()
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kompress-stats-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_run_round_trips_through_the_file() {
        let dir = temp_dir("round-trip");
        let path = dir.join(FILE);

        let mut first = run(1_761_264_000, 2, 952, 735);
        first.planned = 45;
        first.skipped.insert("structured content".into(), 42);
        first.saved.push(MessageSave {
            hash: hash_text("hello world"),
            before: 420,
            after: 300,
        });
        append(&path, &first, MAX_BYTES).unwrap();
        append(&path, &run(1_761_264_100, 1, 100, 80), MAX_BYTES).unwrap();

        let history = History::load(&[path]);
        assert_eq!(history.malformed, 0);
        assert!(history.unreadable.is_empty());
        assert_eq!(history.runs, vec![first, run(1_761_264_100, 1, 100, 80)]);
    }

    #[test]
    fn a_partial_line_is_skipped_and_counted_not_fatal() {
        // What a killed process or two interleaved appends leave behind.
        let dir = temp_dir("partial");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE);
        append(&path, &run(1_761_264_000, 1, 100, 80), MAX_BYTES).unwrap();
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str("{\"ts\":17,\"messages\":3\n");
        fs::write(&path, text).unwrap();

        let history = History::load(&[path]);
        assert_eq!(history.runs.len(), 1);
        assert_eq!(history.malformed, 1);
    }

    #[test]
    fn a_missing_file_reads_as_nothing() {
        // A file that does not exist is "nothing recorded", not an error.
        let history = History::load(&[temp_dir("missing").join(FILE)]);
        assert!(history.runs.is_empty());
        assert_eq!(history.malformed, 0);
        assert!(history.unreadable.is_empty());
    }

    #[test]
    fn the_file_rotates_once_past_the_cap_and_the_oldest_run_survives_one_generation() {
        let dir = temp_dir("rotate");
        let path = dir.join(FILE);
        // A cap of one byte rotates before every second append.
        append(&path, &run(1, 1, 10, 9), 1).unwrap();
        append(&path, &run(2, 1, 10, 8), 1).unwrap();
        append(&path, &run(3, 1, 10, 7), 1).unwrap();

        let history = History::load(&[rotated(&path), path.clone()]);
        let ts: Vec<u64> = history.runs.iter().map(|r| r.ts).collect();
        assert_eq!(ts, vec![2, 3], "only the newest generation is kept");
        assert!(rotated(&path).exists());
        assert!(fs::metadata(&path).unwrap().len() < 200);
    }

    #[test]
    fn concurrent_runs_append_whole_lines() {
        // klava runs one process per request, so several finish at once. Every
        // line must stay parseable: a torn write would be reported as
        // "unreadable" and silently lose that run's tokens.
        let dir = temp_dir("concurrent");
        let path = dir.join(FILE);
        std::thread::scope(|scope| {
            for thread in 0..8 {
                let path = path.clone();
                scope.spawn(move || {
                    for run_index in 0..25 {
                        let mut run = run(thread * 100 + run_index, 1, 500, 400);
                        run.saved.push(MessageSave {
                            hash: hash_text(&format!("message {thread}-{run_index}")),
                            before: 500,
                            after: 400,
                        });
                        append(&path, &run, MAX_BYTES).unwrap();
                    }
                });
            }
        });

        let history = History::load(&[path]);
        assert_eq!(history.malformed, 0, "no line may be torn");
        assert_eq!(history.runs.len(), 200);
        let agg = Aggregate::of(&history.runs);
        assert_eq!(agg.unique_messages, 200);
    }

    #[test]
    fn a_file_that_exists_but_cannot_be_read_is_reported_not_ignored() {
        // Printing "no runs recorded yet" over an unreadable file would claim
        // the data does not exist when it is only inaccessible.
        let dir = temp_dir("unreadable");
        fs::create_dir_all(&dir).unwrap();
        // A directory where the file should be: it exists, and cannot be read
        // as text.
        let history = History::load(&[dir]);
        assert!(history.runs.is_empty());
        assert_eq!(history.unreadable.len(), 1);
        assert!(!history.unreadable[0].1.is_empty());
    }

    #[test]
    fn an_unwritable_location_is_an_error_not_a_panic() {
        // A regular file where the directory should be: `create_dir_all` fails
        // on every platform, including when the tests run as root.
        let dir = temp_dir("unwritable");
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("blocker");
        fs::write(&file, b"x").unwrap();

        assert!(append(&file.join(FILE), &run(1, 1, 10, 9), MAX_BYTES).is_err());
    }

    #[test]
    fn identical_content_hashes_equal_and_different_content_differs() {
        assert_eq!(hash_text("a log line"), hash_text("a log line"));
        assert_ne!(hash_text("a log line"), hash_text("a log line "));
        assert_eq!(hash_text("").len(), 32);
    }

    #[test]
    fn the_same_message_in_two_runs_is_counted_once() {
        let mut first = run(100, 2, 1_000, 800);
        first.saved.push(MessageSave {
            hash: hash_text("repeats every turn"),
            before: 700,
            after: 500,
        });
        first.saved.push(MessageSave {
            hash: hash_text("seen once"),
            before: 300,
            after: 300,
        });

        // The same tool output, compressed again on the next turn, plus one new
        // message.
        let mut second = run(200, 2, 1_000, 810);
        second.saved.push(MessageSave {
            hash: hash_text("repeats every turn"),
            before: 700,
            after: 500,
        });
        second.saved.push(MessageSave {
            hash: hash_text("new"),
            before: 300,
            after: 290,
        });

        let agg = Aggregate::of(&[first, second]);
        assert_eq!(agg.runs, 2);
        assert_eq!(agg.runs_compressed, 2);
        assert_eq!(agg.messages, 4);
        assert_eq!(agg.saved(), 390, "per-run totals add up");

        // Two distinct messages: "seen once" is kept as a zero-saving entry
        // because it was rewritten (500 -> 500 was rejected upstream, but the
        // aggregation must still be arithmetic, not a filter).
        assert_eq!(agg.unique_messages, 3);
        assert_eq!(agg.unique_before, 1_300);
        assert_eq!(agg.unique_after, 1_090);
        assert_eq!(agg.unique_saved(), 210);
    }

    #[test]
    fn the_newest_result_for_a_message_wins() {
        let mut older = run(100, 1, 500, 300);
        older.saved.push(MessageSave {
            hash: hash_text("same"),
            before: 500,
            after: 300,
        });
        let mut newer = run(200, 1, 500, 400);
        newer.saved.push(MessageSave {
            hash: hash_text("same"),
            before: 500,
            after: 400,
        });

        let agg = Aggregate::of(&[older, newer]);
        assert_eq!(agg.unique_messages, 1);
        assert_eq!(agg.unique_saved(), 100, "a stricter threshold counts once");
    }

    #[test]
    fn runs_that_compressed_nothing_contribute_skips_but_no_token_totals() {
        let mut idle = RunRecord::new();
        idle.planned = 0;
        idle.skipped.insert("shorter than the byte floor".into(), 7);
        let mut busy = run(1_761_264_000, 1, 400, 300);
        busy.skipped.insert("structured content".into(), 2);

        let agg = Aggregate::of(&[idle, busy]);
        assert_eq!(agg.runs, 2);
        assert_eq!(agg.runs_compressed, 1, "a pass-through run is not a saving");
        assert_eq!(agg.messages, 1);
        assert_eq!(agg.before, 400);
        assert_eq!(agg.saved(), 100);
        assert_eq!(agg.skipped["shorter than the byte floor"], 7);
        assert_eq!(agg.skipped["structured content"], 2);
    }

    #[test]
    fn the_per_day_table_groups_by_utc_date() {
        let agg = Aggregate::of(&[
            run(1_761_264_000, 1, 100, 90),
            run(1_761_264_001, 1, 200, 100),
            run(1_761_350_400, 2, 400, 100),
        ]);
        assert_eq!(agg.days.len(), 2);
        let first = &agg.days["2025-10-24"];
        assert_eq!((first.runs, first.messages), (2, 2));
        assert_eq!((first.before, first.after), (300, 190));
        let second = &agg.days["2025-10-25"];
        assert_eq!((second.runs, second.messages), (1, 2));
        assert_eq!(agg.first_ts, Some(1_761_264_000));
        assert_eq!(agg.last_ts, Some(1_761_350_400));
    }

    #[test]
    fn an_empty_history_totals_to_zero_rather_than_failing() {
        let agg = Aggregate::of(&[]);
        assert_eq!(agg.runs, 0);
        assert_eq!(agg.saved(), 0);
        assert_eq!(agg.unique_saved(), 0);
        assert!(agg.days.is_empty());
        assert_eq!(agg.first_ts, None);
    }

    #[test]
    fn dates_are_utc_civil_dates() {
        assert_eq!(date_of(0), "1970-01-01");
        // Midnight exactly, and the second before it.
        assert_eq!(date_of(1_761_264_000), "2025-10-24");
        assert_eq!(date_of(1_761_263_999), "2025-10-23");
        // Leap day, and the day after.
        assert_eq!(date_of(1_709_164_800), "2024-02-29");
        assert_eq!(date_of(1_709_251_200), "2024-03-01");
        // A century that is a leap year, which the /4 /100 /400 rules get wrong
        // when one of them is dropped.
        assert_eq!(date_of(951_782_400), "2000-02-29");
    }
}
