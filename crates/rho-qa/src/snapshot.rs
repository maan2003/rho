//! Taking a snapshot of the user's state while the daemon runs.
//!
//! The copy is hot: the live daemon keeps writing throughout, and a proof does
//! not need a quiesced copy. What a hot copy of a redb file can be is torn —
//! the header committed at one moment, pages read at another — so a copy is
//! not a snapshot until it has been opened and read. Verification runs on the
//! copy, which is also where redb's own recovery runs, so a verified snapshot
//! is a store that opens cleanly. A file that fails is copied once more before
//! the whole snapshot is called failed.

use std::path::{Path, PathBuf};
use std::time::Instant;
use std::{fs, io};

use anyhow::{Context as _, Result, bail};
use clap::Args;
use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Args)]
pub struct SnapshotArgs {
    /// The snapshot's name. The date is appended.
    #[arg(long, default_value = "user")]
    name: String,

    /// The state directory to copy. Read only, never written.
    #[arg(long)]
    source: Option<PathBuf>,

    /// Where snapshots are kept.
    #[arg(long)]
    root: Option<PathBuf>,

    /// Skip opening the copies to check they read. Faster, and worth less.
    #[arg(long)]
    no_verify: bool,
}

/// What a snapshot is, written next to the copy as `manifest.json`. A bug
/// report is "this snapshot, this action", so the manifest has to say enough
/// to identify both the state and the tree that took it.
#[derive(Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub taken_at: String,
    pub source: PathBuf,
    pub host: String,
    /// The commit of the working tree that took the snapshot.
    pub tree_commit: Option<String>,
    pub files: Vec<FileRecord>,
    pub databases: Vec<DatabaseRecord>,
    pub total_bytes: u64,
    pub seconds: f64,
}

#[derive(Serialize, Deserialize)]
pub struct FileRecord {
    pub relative: String,
    pub bytes: u64,
}

/// What one copied database holds, read back from the copy. These numbers are
/// the size of the user's world: the rows a QA run is actually working over.
#[derive(Serialize, Deserialize)]
pub struct DatabaseRecord {
    pub relative: String,
    pub tables: Vec<TableRecord>,
}

#[derive(Serialize, Deserialize)]
pub struct TableRecord {
    pub name: String,
    pub rows: u64,
    pub stored_bytes: u64,
}

pub fn take(args: SnapshotArgs) -> Result<()> {
    let source = match args.source {
        Some(dir) => dir,
        None => paths::live_state()?,
    };
    if !source.is_dir() {
        bail!("no state directory at {}", source.display());
    }
    let root = match args.root {
        Some(dir) => dir,
        None => paths::snapshots_root()?,
    };

    let today = chrono::Local::now();
    let dir = root.join(format!("{}-{}", args.name, today.format("%Y-%m-%d")));
    if dir.exists() {
        bail!(
            "{} already exists; snapshots are never overwritten, pass another --name",
            dir.display()
        );
    }
    let state = dir.join("state").join("rho");
    fs::create_dir_all(&state)
        .with_context(|| format!("create snapshot directory {}", state.display()))?;

    let started = Instant::now();
    let mut files = Vec::new();
    let mut total_bytes = 0;
    for relative in paths::SNAPSHOT_CONTENTS {
        let from = source.join(relative);
        if !from.exists() {
            println!("skip    {relative} (not in the live state)");
            continue;
        }
        let bytes = copy_file(&from, &state.join(relative))?;
        total_bytes += bytes;
        files.push(FileRecord {
            relative: (*relative).to_owned(),
            bytes,
        });
        println!("copied  {relative} ({})", human(bytes));
    }

    let mut databases = Vec::new();
    if !args.no_verify {
        for record in &files {
            if !record.relative.ends_with(".redb") {
                continue;
            }
            let copy = state.join(&record.relative);
            let tables = match read_tables(&copy) {
                Ok(tables) => tables,
                Err(error) => {
                    // A torn copy: take that one file again, from the live
                    // file as it stands now, and read it once more.
                    println!("torn    {}: {error:#}; copying again", record.relative);
                    copy_file(&source.join(&record.relative), &copy)?;
                    read_tables(&copy).with_context(|| {
                        format!("{} does not read after a second copy", record.relative)
                    })?
                }
            };
            let rows: u64 = tables.iter().map(|table| table.rows).sum();
            println!(
                "read    {} ({rows} rows in {} tables)",
                record.relative,
                tables.len()
            );
            databases.push(DatabaseRecord {
                relative: record.relative.clone(),
                tables,
            });
        }
    }

    let manifest = Manifest {
        name: args.name,
        taken_at: today.to_rfc3339(),
        source,
        host: hostname(),
        tree_commit: tree_commit(),
        files,
        databases,
        total_bytes,
        seconds: started.elapsed().as_secs_f64(),
    };
    let path = dir.join("manifest.json");
    fs::write(&path, serde_json::to_string_pretty(&manifest)?)
        .with_context(|| format!("write {}", path.display()))?;

    println!(
        "\n{} — {} in {:.1}s",
        dir.display(),
        human(total_bytes),
        manifest.seconds
    );
    Ok(())
}

pub fn list() -> Result<()> {
    let root = paths::snapshots_root()?;
    let mut entries: Vec<_> = match fs::read_dir(&root) {
        Ok(entries) => entries.collect::<io::Result<Vec<_>>>()?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            println!("no snapshots yet ({} does not exist)", root.display());
            return Ok(());
        }
        Err(error) => return Err(error).context(format!("read {}", root.display())),
    };
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let manifest = entry.path().join("manifest.json");
        let Ok(text) = fs::read_to_string(&manifest) else {
            println!("{}  (no manifest; incomplete)", entry.file_name().display());
            continue;
        };
        let manifest: Manifest =
            serde_json::from_str(&text).with_context(|| format!("parse {}", manifest.display()))?;
        let rows: u64 = manifest
            .databases
            .iter()
            .flat_map(|database| &database.tables)
            .map(|table| table.rows)
            .sum();
        println!(
            "{}  {}  {rows} rows  taken {}",
            entry.file_name().display(),
            human(manifest.total_bytes),
            manifest.taken_at,
        );
    }
    Ok(())
}

/// Open a copied database and read what it holds. This is the verification:
/// a file that opens, lists its tables and counts their rows is a file a rig
/// daemon can run on. Only the copy is ever opened.
fn read_tables(path: &Path) -> Result<Vec<TableRecord>> {
    use redb::{ReadableDatabase as _, ReadableTableMetadata as _, TableHandle as _};

    let database = redb::Database::open(path)?;
    let read = database.begin_read()?;
    let mut tables = Vec::new();
    for handle in read.list_tables()? {
        let name = handle.name().to_owned();
        let table = read.open_untyped_table(handle)?;
        let stats = table.stats()?;
        tables.push(TableRecord {
            name,
            rows: table.len()?,
            stored_bytes: stats.stored_bytes(),
        });
    }
    tables.sort_by_key(|table| std::cmp::Reverse(table.stored_bytes));
    Ok(tables)
}

fn copy_file(from: &Path, to: &Path) -> Result<u64> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(from, to).with_context(|| format!("copy {} to {}", from.display(), to.display()))
}

fn hostname() -> String {
    fs::read_to_string("/etc/hostname")
        .map(|name| name.trim().to_owned())
        .unwrap_or_default()
}

/// The commit of the tree this binary was run from, so a snapshot says which
/// rho took it. jj first, git after; neither is required.
pub fn tree_commit() -> Option<String> {
    let jj = std::process::Command::new("jj")
        .args(["log", "-r", "@", "--no-graph", "-T", "commit_id.short()"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|id| !id.is_empty());
    jj.or_else(|| {
        std::process::Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .filter(|id| !id.is_empty())
    })
}

pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
