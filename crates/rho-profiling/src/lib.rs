//! Opt-in, unprivileged CPU profiling shared by Rho executables.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};

use anyhow::Context as _;

pub struct CpuProfiler {
    path: PathBuf,
    guard: pprof::ProfilerGuard<'static>,
}

impl CpuProfiler {
    pub fn start(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let path = absolute_path(path.into())?;
        let guard = pprof::ProfilerGuardBuilder::default()
            .frequency(100)
            .build()
            .context("start in-process CPU profiler")?;
        Ok(Self { path, guard })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn finish(self) -> anyhow::Result<PathBuf> {
        let report = self.guard.report().build().context("build CPU profile")?;
        let mut stacks = report
            .data
            .iter()
            .map(|(frames, count)| {
                let mut stack = String::new();
                push_folded_name(&mut stack, &frames.thread_name_or_id());
                for frame in frames.frames.iter().rev() {
                    for symbol in frame.iter().rev() {
                        stack.push(';');
                        push_folded_name(&mut stack, &symbol.name());
                        if symbol.lineno() != 0 {
                            write!(stack, " [{}:{}]", symbol.filename(), symbol.lineno()).unwrap();
                        }
                    }
                }
                (stack, count)
            })
            .collect::<Vec<_>>();
        stacks.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));

        let file = File::create(&self.path)
            .with_context(|| format!("create CPU profile {}", self.path.display()))?;
        let mut writer = BufWriter::new(file);
        for (stack, count) in stacks {
            writeln!(writer, "{stack} {count}")
                .with_context(|| format!("write CPU profile {}", self.path.display()))?;
        }
        Ok(self.path)
    }
}

pub fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut path = path.as_os_str().to_owned();
    path.push(suffix);
    path.into()
}

fn absolute_path(path: PathBuf) -> anyhow::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()
            .context("read current directory for CPU profile")?
            .join(path))
    }
}

fn push_folded_name(output: &mut String, name: &str) {
    output.extend(name.chars().map(|character| match character {
        ';' | '\n' | '\r' => ' ',
        character => character,
    }));
}

#[cfg(test)]
mod tests {
    #[test]
    fn writes_an_unprivileged_folded_profile() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cpu.folded");
        let profiler = super::CpuProfiler::start(&path).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(50);
        while std::time::Instant::now() < deadline {
            std::hint::black_box((1..100).sum::<u64>());
        }
        assert_eq!(profiler.finish().unwrap(), path);
        assert!(!std::fs::read_to_string(path).unwrap().is_empty());
    }
}
