#[derive(Debug, Clone)]
pub struct PathSegment(String);

impl PathSegment {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> serde::Deserialize<'de> for PathSegment {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        if s.is_empty() {
            return Err(serde::de::Error::custom("path segment cannot be empty"));
        }
        if !s
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(serde::de::Error::custom(
                "invalid characters in path segment",
            ));
        }
        Ok(PathSegment(s))
    }
}
