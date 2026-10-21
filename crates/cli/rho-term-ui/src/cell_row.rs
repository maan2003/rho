// Derived from dpc/tau (crates/tau-term-screen/src/cell_row.rs), MPL-2.0.
// This file remains under MPL-2.0.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::ops::Deref;
use std::sync::Arc;

use crate::Cell;

/// One normalized, immutable, cheaply shared physical terminal row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellRow {
    /// Shared normalized cells for this physical row.
    cells: CellRowStorage,
}

/// Allocation-free empty storage or shared nonempty cells.
#[derive(Clone, Debug, PartialEq, Eq)]
enum CellRowStorage {
    /// Canonical allocation-free empty row.
    Empty,
    /// Shared nonempty row cells.
    Cells(Arc<Vec<Cell>>),
}

impl CellRow {
    /// Normalizes and takes ownership of one physical terminal row.
    pub fn new(mut cells: Vec<Cell>) -> Self {
        if cells.is_empty() {
            return Self {
                cells: CellRowStorage::Empty,
            };
        }
        if cells
            .iter()
            .any(|cell| Cell::sanitized_char(cell.ch) != cell.ch)
        {
            cells = cells.iter().map(Cell::normalized).collect();
        }
        Self {
            cells: CellRowStorage::Cells(Arc::new(cells)),
        }
    }
}

impl Deref for CellRow {
    type Target = [Cell];

    fn deref(&self) -> &Self::Target {
        match &self.cells {
            CellRowStorage::Empty => &[],
            CellRowStorage::Cells(cells) => cells.as_slice(),
        }
    }
}

impl AsRef<[Cell]> for CellRow {
    fn as_ref(&self) -> &[Cell] {
        self
    }
}

impl From<Vec<Cell>> for CellRow {
    fn from(cells: Vec<Cell>) -> Self {
        Self::new(cells)
    }
}

impl PartialEq<Vec<Cell>> for CellRow {
    fn eq(&self, other: &Vec<Cell>) -> bool {
        self.as_ref() == other.as_slice()
    }
}
