//! The sprite atlas, kept as a lookup: no pixels are stored, but each tile
//! remembers what it stands for, so a sprite in the Scene can be read back
//! as the character (and its font's weight and slant) gpui painted.

use std::borrow::Cow;
use std::collections::HashMap;

use gpui::{
    AtlasKey, AtlasTextureId, AtlasTile, Bounds, DevicePixels, PlatformAtlas, Point, Result, Size,
    TileId,
};
use parking_lot::Mutex;

/// What a tile shows in a cell.
#[derive(Clone, Copy, Debug)]
pub(crate) enum TileContent {
    Glyph {
        ch: char,
        font_id: usize,
    },
    /// An icon or image: drawn as a placeholder, if at all.
    Picture,
}

#[derive(Default)]
struct State {
    tiles: HashMap<AtlasKey, AtlasTile>,
    contents: HashMap<u32, TileContent>,
    next: u32,
}

#[derive(Default)]
pub(crate) struct CellAtlas(Mutex<State>);

impl CellAtlas {
    pub(crate) fn content(&self, tile: TileId) -> Option<TileContent> {
        self.0.lock().contents.get(&tile.0).copied()
    }
}

impl PlatformAtlas for CellAtlas {
    fn get_or_insert_with<'a>(
        &self,
        key: AtlasKey,
        build: &mut dyn FnMut() -> Result<Option<(Size<DevicePixels>, Cow<'a, [u8]>)>>,
    ) -> Result<Option<AtlasTile>> {
        let mut state = self.0.lock();
        if let Some(tile) = state.tiles.get(&key) {
            return Ok(Some(*tile));
        }
        let Some((size, _bytes)) = build()? else {
            return Ok(None);
        };
        state.next += 1;
        let id = state.next;
        let content = match &key {
            AtlasKey::Glyph(params) => TileContent::Glyph {
                ch: char::from_u32(params.glyph_id.0).unwrap_or('\u{fffd}'),
                font_id: params.font_id.0,
            },
            _ => TileContent::Picture,
        };
        let tile = AtlasTile {
            texture_id: AtlasTextureId {
                index: 0,
                kind: key.texture_kind(),
            },
            tile_id: TileId(id),
            padding: 0,
            bounds: Bounds {
                origin: Point::default(),
                size,
            },
        };
        state.contents.insert(id, content);
        state.tiles.insert(key, tile);
        Ok(Some(tile))
    }

    fn remove(&self, key: &AtlasKey) {
        let mut state = self.0.lock();
        if let Some(tile) = state.tiles.remove(key) {
            state.contents.remove(&tile.tile_id.0);
        }
    }
}
