use senax_encoder::{Decode, Encode};

#[derive(
    Clone, Copy, Debug, Decode, Encode, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize,
)]
pub struct PromptCacheKey([u8; 8]);

impl PromptCacheKey {
    pub fn generate() -> Self {
        let mut bytes = [0; 8];
        rand::Rng::fill_bytes(&mut rand::rng(), &mut bytes);
        Self(bytes)
    }

    pub fn to_bytes(self) -> [u8; 8] {
        self.0
    }
}
