/// Defines a newtype around `Arc<str>` whose contents are checked by
/// `$validate` on every construction. The field is private and there is no
/// unchecked constructor, so a value of this type is guaranteed valid — including
/// when it comes off the wire, since `Deserialize` is implemented by hand to run
/// the same validation rather than deriving a bypass.
macro_rules! validated_string_type {
    ($(#[$meta:meta])* $vis:vis $name:ident, $validate:expr) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize)]
        #[serde(transparent)]
        $vis struct $name(std::sync::Arc<str>);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<std::sync::Arc<str>> for $name {
            type Error = anyhow::Error;
            fn try_from(id: std::sync::Arc<str>) -> anyhow::Result<Self> {
                $validate(&id)?;
                Ok(Self(id))
            }
        }

        impl TryFrom<&'_ str> for $name {
            type Error = anyhow::Error;
            fn try_from(id: &str) -> anyhow::Result<Self> {
                $validate(id)?;
                Ok(Self(std::sync::Arc::from(id)))
            }
        }

        impl TryFrom<String> for $name {
            type Error = anyhow::Error;
            fn try_from(id: String) -> anyhow::Result<Self> {
                $validate(&id)?;
                Ok(Self(std::sync::Arc::from(id)))
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let raw = <std::sync::Arc<str> as serde::Deserialize>::deserialize(deserializer)?;
                <$name>::try_from(raw).map_err(serde::de::Error::custom)
            }
        }
    };
}

pub(crate) use validated_string_type;

// A-Za-z0-9_-
pub(crate) fn validate_identifier(value: &str) -> anyhow::Result<()> {
    if value.is_empty() {
        anyhow::bail!("identifier must not be empty");
    }
    if let Some(b) = value
        .bytes()
        .find(|b| !(b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-'))
    {
        anyhow::bail!(
            "identifier {value:?} contains invalid character {:?}; only A-Za-z0-9, '_' and '-' are allowed",
            b as char
        );
    }
    Ok(())
}
