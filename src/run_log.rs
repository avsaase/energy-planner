//! A complete record of every replan, for debugging: the optimizer input, the live
//! measurements and nowcast it was based on, and the resulting plan (or the solver error).
//!
//! One JSON line per replan, in a gzip file per local day (`runs/2026-11-08.jsonl.gz` in the
//! data directory). Every line is its own gzip member, so appending never rewrites the file;
//! `gzip -dc`, `zcat` and Python's `gzip` module read the members as one stream. Files older
//! than the configured number of days are deleted.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
};

use flate2::{Compression, write::GzEncoder};
use jiff::{ToSpan, Zoned, civil::Date};
use serde::Serialize;

use crate::{
    data_dir,
    nowcast::Nowcast,
    types::{InputData, Planning},
};

const EXTENSION: &str = ".jsonl.gz";

#[derive(Serialize)]
pub struct RunRecord<'a> {
    pub planned_at: &'a Zoned,
    /// Live house load and net load when the replan started.
    pub measured_house_load_w: Option<f64>,
    pub measured_net_load_w: Option<f64>,
    pub nowcast: Option<Nowcast>,
    pub input: &'a InputData,
    pub planning: Option<&'a Planning>,
    pub error: Option<String>,
    pub prepare_ms: u128,
    pub solve_ms: u128,
}

pub struct RunLog {
    dir: PathBuf,
    keep_days: u32,
}

impl RunLog {
    /// Keeps `keep_days` days of records (today included); 0 disables the log.
    pub fn new(keep_days: u32) -> Self {
        Self {
            dir: runs_dir(),
            keep_days,
        }
    }

    pub fn append(&self, record: &RunRecord) -> anyhow::Result<()> {
        if self.keep_days == 0 {
            return Ok(());
        }
        fs::create_dir_all(&self.dir)?;
        let today = record.planned_at.date();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(file_name(today)))?;
        let mut gz = GzEncoder::new(file, Compression::default());
        serde_json::to_writer(&mut gz, record)?;
        gz.write_all(b"\n")?;
        gz.finish()?;
        self.prune(today)
    }

    fn prune(&self, today: Date) -> anyhow::Result<()> {
        let oldest_kept = today.checked_sub((i64::from(self.keep_days) - 1).days())?;
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            let date = path.file_name().and_then(|n| n.to_str()).and_then(date_of);
            if date.is_some_and(|d| d < oldest_kept) {
                fs::remove_file(&path)?;
            }
        }
        Ok(())
    }
}

fn runs_dir() -> PathBuf {
    data_dir().join("runs")
}

fn file_name(date: Date) -> String {
    format!("{date}{EXTENSION}")
}

/// The day of a run log file name; `None` for anything else (including path tricks).
fn date_of(name: &str) -> Option<Date> {
    let date: Date = name.strip_suffix(EXTENSION)?.parse().ok()?;
    (file_name(date) == name).then_some(date)
}

/// Run log files as `(name, size in bytes)`, newest first.
pub fn list_files() -> Vec<(String, u64)> {
    let Ok(entries) = fs::read_dir(runs_dir()) else {
        return Vec::new();
    };
    let mut files: Vec<(String, u64)> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().into_string().ok()?;
            date_of(&name)?;
            Some((name, entry.metadata().ok()?.len()))
        })
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    files
}

/// Path of the run log file `name`, if it is a valid run log file name.
pub fn file_path(name: &str) -> Option<PathBuf> {
    date_of(name).map(|_| runs_dir().join(name))
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use flate2::read::MultiGzDecoder;
    use jiff::civil::date;

    use super::*;

    #[test]
    fn only_run_log_names_are_accepted() {
        assert_eq!(date_of("2026-11-08.jsonl.gz"), Some(date(2026, 11, 8)));
        assert_eq!(date_of("../2026-11-08.jsonl.gz"), None);
        assert_eq!(date_of("2026-11-08.jsonl"), None);
        assert_eq!(date_of("options.json"), None);
        assert!(file_path("../options.json").is_none());
    }

    #[test]
    fn appended_members_read_as_one_stream_and_old_days_are_pruned() {
        let dir = std::env::temp_dir().join(format!("run-log-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let log = RunLog {
            dir: dir.clone(),
            keep_days: 2,
        };
        fs::write(dir.join("2026-11-01.jsonl.gz"), b"old").unwrap();
        fs::write(dir.join("2026-11-07.jsonl.gz"), b"yesterday").unwrap();
        fs::write(dir.join("notes.txt"), b"not ours").unwrap();

        let path = dir.join("2026-11-08.jsonl.gz");
        for line in ["{\"a\":1}", "{\"a\":2}"] {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap();
            let mut gz = GzEncoder::new(file, Compression::default());
            writeln!(gz, "{line}").unwrap();
            gz.finish().unwrap();
        }
        log.prune(date(2026, 11, 8)).unwrap();

        let mut text = String::new();
        MultiGzDecoder::new(fs::File::open(&path).unwrap())
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "{\"a\":1}\n{\"a\":2}\n");
        assert!(!dir.join("2026-11-01.jsonl.gz").exists());
        assert!(dir.join("2026-11-07.jsonl.gz").exists());
        assert!(dir.join("notes.txt").exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
