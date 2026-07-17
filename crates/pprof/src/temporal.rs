// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

//! Timestamped samples collected alongside the aggregate pprof report.

use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct TemporalSample {
    pub monotonic_ns: u64,
    pub tid: u64,
    pub stack_id: u32,
}

#[derive(Clone, Debug)]
pub struct TemporalStack {
    /// Instruction pointers ordered from leaf to root.
    pub instruction_pointers: Vec<usize>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Default)]
pub struct TemporalReport {
    pub samples: Vec<TemporalSample>,
    pub stacks: Vec<TemporalStack>,
    pub thread_names: HashMap<u64, String>,
    pub dropped_samples: u64,
}

#[derive(Hash, PartialEq, Eq)]
struct StackKey {
    instruction_pointers: Vec<usize>,
    truncated: bool,
}

#[derive(Default)]
pub(crate) struct TemporalArchive {
    report: TemporalReport,
    stack_ids: HashMap<StackKey, u32>,
}

impl TemporalArchive {
    pub(crate) fn push(
        &mut self,
        monotonic_ns: u64,
        tid: u64,
        thread_name: &[u8],
        instruction_pointers: Vec<usize>,
        truncated: bool,
    ) {
        let key = StackKey {
            instruction_pointers,
            truncated,
        };
        let stack_id = match self.stack_ids.get(&key) {
            Some(stack_id) => *stack_id,
            None => {
                let stack_id = self.report.stacks.len() as u32;
                self.report.stacks.push(TemporalStack {
                    instruction_pointers: key.instruction_pointers.clone(),
                    truncated,
                });
                self.stack_ids.insert(key, stack_id);
                stack_id
            }
        };
        self.report.samples.push(TemporalSample {
            monotonic_ns,
            tid,
            stack_id,
        });
        self.report
            .thread_names
            .entry(tid)
            .or_insert_with(|| String::from_utf8_lossy(thread_name).into_owned());
    }

    pub(crate) fn add_dropped(&mut self, dropped: u64) {
        self.report.dropped_samples = self.report.dropped_samples.saturating_add(dropped);
    }

    pub(crate) fn report(&self) -> TemporalReport {
        self.report.clone()
    }
}
