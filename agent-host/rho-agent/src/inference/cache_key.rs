use senax_encoder::{Decode, Encode};

#[derive(
    Clone, Copy, Debug, Decode, Encode, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize,
)]
pub struct PromptCacheKey([u8; 8]);

impl PromptCacheKey {
    pub fn generate() -> Self {
        let mut bytes = [0; 8];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut bytes);
        Self(bytes)
    }

    pub fn to_bytes(self) -> [u8; 8] {
        self.0
    }

    pub fn debug_file_stem(self) -> String {
        let mut stem = String::with_capacity(self.0.len() * 2);
        for byte in self.0 {
            use std::fmt::Write as _;
            let _ = write!(&mut stem, "{byte:02x}");
        }
        stem
    }
}
