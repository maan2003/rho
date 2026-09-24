//! The tables the client's copy of the desk was kept in, as far as the
//! migration into the ledger reads them.

use redb::TableDefinition;
use rho_db::Sen;

use crate::protocol::cells::{BodySnapshot, Cell, Id, PropertyKey};

/// One cell per row, at the key the store itself used. A delete is a
/// `Deleted(true)` cell like any other.
pub(crate) const DESK_CELLS: TableDefinition<Sen<CellKey>, Sen<Cell>> =
    TableDefinition::new("gui_desk_cells_v1");
/// A note's text, as the operations that made it.
pub(crate) const DESK_BODIES: TableDefinition<Sen<BodyKey>, Sen<BodySnapshot>> =
    TableDefinition::new("gui_desk_bodies_v1");

#[derive(Clone, Debug, senax_encoder::Encode, senax_encoder::Decode)]
pub(crate) struct CellKey {
    pub(crate) host: String,
    id: Id,
    property: PropertyKey,
}

#[derive(Clone, Debug, senax_encoder::Encode, senax_encoder::Decode)]
pub(crate) struct BodyKey {
    pub(crate) host: String,
    pub(crate) id: Id,
}
