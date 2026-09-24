//! The user's secret: one phrase, kept by every device they use and in
//! their password manager, that every key rho needs is derived from.
//!
//! It is twelve words from the BIP39 English list, which is sixteen bytes
//! of randomness and a checksum: short enough to type, and a mistyped word
//! is refused rather than read as some other secret. Each use takes its
//! own key from it under its own context, so a new use needs no new secret
//! and no use can read another's.

use bip39::{Language, Mnemonic};

/// How many words a phrase has.
pub const WORDS: usize = 12;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Secret(pub [u8; 16]);

impl Secret {
    pub fn generate() -> Self {
        use rand::RngCore as _;
        let mut entropy = [0; 16];
        rand::rngs::OsRng.fill_bytes(&mut entropy);
        Self(entropy)
    }

    /// The phrase, as the user writes it down.
    pub fn to_words(&self) -> String {
        let mnemonic = Mnemonic::from_entropy_in(Language::English, &self.0)
            .expect("sixteen bytes are a twelve-word phrase");
        mnemonic.words().collect::<Vec<_>>().join(" ")
    }

    /// Reads a phrase back, in any case and spacing. The answer says what
    /// is wrong with it, for the user to fix.
    pub fn from_words(text: &str) -> Result<Self, String> {
        let words: Vec<String> = text.split_whitespace().map(str::to_lowercase).collect();
        if words.len() != WORDS {
            return Err(format!("{WORDS} words, not {}", words.len()));
        }
        if let Some(word) = words
            .iter()
            .find(|word| Language::English.find_word(word).is_none())
        {
            return Err(format!("`{word}` is not a phrase word"));
        }
        let mnemonic = Mnemonic::parse_in_normalized(Language::English, &words.join(" "))
            .map_err(|_| "a word is wrong: the phrase does not check out".to_owned())?;
        let (entropy, len) = mnemonic.to_entropy_array();
        Ok(Self(
            entropy[..len]
                .try_into()
                .expect("twelve words are sixteen bytes"),
        ))
    }

    /// The phrase words that start with `prefix`, for completing one.
    pub fn words_starting(prefix: &str) -> &[&'static str] {
        Language::English.words_by_prefix(prefix)
    }

    /// The key for one use, named by `context` in BLAKE3's own form:
    /// the application, when the use was made, and what it is for, as in
    /// `rho 2026-09-24 ledger`. A context is never reused for something
    /// else: two uses under one context share a key.
    pub fn derive(&self, context: &str) -> [u8; 32] {
        blake3::derive_key(context, &self.0)
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_phrase_reads_back_as_the_same_secret() {
        let secret = Secret::generate();
        let words = secret.to_words();
        assert_eq!(words.split(' ').count(), WORDS);
        assert_eq!(Secret::from_words(&words), Ok(secret));
        let shouted = format!("  {}  ", words.to_uppercase().replace(' ', "\n "));
        assert_eq!(Secret::from_words(&shouted), Ok(secret));
    }

    #[test]
    fn a_mistyped_phrase_is_refused_and_says_why() {
        let words = Secret::generate().to_words();
        let mut list: Vec<&str> = words.split(' ').collect();
        assert!(Secret::from_words(&list[..11].join(" ")).is_err());
        list[3] = "notaword";
        assert_eq!(
            Secret::from_words(&list.join(" ")),
            Err("`notaword` is not a phrase word".to_owned())
        );
        // Swapping two words keeps every word real and breaks the checksum
        // in all but a sliver of cases; a phrase whose first two words are
        // the same is skipped rather than asserted on.
        let mut swapped: Vec<&str> = words.split(' ').collect();
        if swapped[0] != swapped[1] {
            swapped.swap(0, 1);
            let read = Secret::from_words(&swapped.join(" "));
            assert!(read.is_err() || read != Secret::from_words(&words));
        }
    }

    #[test]
    fn each_context_has_its_own_key() {
        let secret = Secret([7; 16]);
        let ledger = "rho 2026-09-24 ledger";
        assert_eq!(secret.derive(ledger), secret.derive(ledger));
        assert_ne!(
            secret.derive(ledger),
            secret.derive("rho 2026-09-24 backup")
        );
        assert_ne!(Secret([8; 16]).derive(ledger), secret.derive(ledger));
    }
}
