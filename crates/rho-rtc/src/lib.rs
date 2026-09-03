//! Native realtime audio sessions.
//!
//! This crate owns WebRTC and local audio devices. OpenAI provider
//! control traffic is handled out-of-band by `rho-openai-realtime`.

use anyhow::Context as _;

const MAX_SDP_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SdpOffer(String);

impl TryFrom<String> for SdpOffer {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        validate_sdp(&value, "offer")?;
        Ok(Self(value))
    }
}

impl SdpOffer {
    pub fn into_string(self) -> String {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SdpAnswer(String);

impl TryFrom<String> for SdpAnswer {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        validate_sdp(&value, "answer")?;
        Ok(Self(value))
    }
}

fn validate_sdp(value: &str, kind: &str) -> anyhow::Result<()> {
    anyhow::ensure!(value.len() <= MAX_SDP_BYTES, "SDP {kind} is too large");
    anyhow::ensure!(value.starts_with("v=0"), "invalid SDP {kind}");
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RtcEvent {
    Error(String),
    Closed,
}

mod native;
pub use native::RtcSession;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdp_newtypes_validate_at_the_boundary() {
        assert!(SdpOffer::try_from("not sdp".to_owned()).is_err());
        assert!(SdpAnswer::try_from("v=0\r\n".to_owned()).is_ok());
    }
}
