use std::fmt;
use std::str::FromStr;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A durable logical Chrome tab identity owned by the Rho browser extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PageId(pub Uuid);

impl fmt::Display for PageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "web-{}", self.0)
    }
}

impl FromStr for PageId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(
            value.strip_prefix("web-").unwrap_or(value),
        )?))
    }
}

/// Browser-owned metadata returned when the extension creates or lists a page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageRecord {
    pub id: PageId,
    pub launch_url: String,
    pub created_at_ms: u64,
}

pub(crate) fn validate_launch_url(target: &str) -> Result<()> {
    const MAX_URL_BYTES: usize = 4096;
    if target.len() > MAX_URL_BYTES {
        bail!("browser URL exceeds {MAX_URL_BYTES} bytes")
    }
    let url = url::Url::parse(target)?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("browser URL must use http or https")
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_ids_round_trip_as_desk_tags() {
        let id = PageId(Uuid::parse_str("018fbe9a-4e55-7b7d-a57e-7f7f4f31d1cd").unwrap());
        assert_eq!(id.to_string().parse(), Ok(id));
        assert_eq!(id.0.to_string().parse(), Ok(id));
        assert!("web-not-a-uuid".parse::<PageId>().is_err());
    }

    #[test]
    fn accepts_only_bounded_web_urls() {
        assert!(validate_launch_url("https://example.com/path").is_ok());
        assert!(validate_launch_url("file:///etc/passwd").is_err());
        assert!(validate_launch_url("javascript:alert(1)").is_err());
        assert!(validate_launch_url(&format!("https://example.com/{}", "x".repeat(8192))).is_err());
    }
}
