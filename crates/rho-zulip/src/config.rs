use std::path::PathBuf;

use anyhow::Context as _;

/// Credentials read from Zulip's standard `zuliprc` file.
pub struct Credentials {
    pub site: String,
    pub email: String,
    pub key: String,
}

impl Credentials {
    pub fn parse(contents: &str) -> anyhow::Result<Self> {
        let mut in_api = false;
        let mut email = None;
        let mut key = None;
        let mut site = None;

        for line in contents.lines() {
            let line = line.split(['#', ';']).next().unwrap_or_default().trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with('[') && line.ends_with(']') {
                in_api = line[1..line.len() - 1].trim().eq_ignore_ascii_case("api");
                continue;
            }
            if !in_api {
                continue;
            }
            let Some((name, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim().to_owned();
            match name.trim() {
                "email" => email = Some(value),
                "key" => key = Some(value),
                "site" => site = Some(value),
                _ => {}
            }
        }

        let site = normalize_site(&site.context("zuliprc [api] section is missing site")?)?;
        let email = email.context("zuliprc [api] section is missing email")?;
        let key = key.context("zuliprc [api] section is missing key")?;
        if email.is_empty() || key.is_empty() {
            anyhow::bail!("zuliprc [api] section has an empty credential");
        }
        Ok(Self { site, email, key })
    }

    pub fn discover() -> anyhow::Result<Self> {
        let path = std::env::var_os("ZULIPRC")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|home| home.join(".zuliprc")))
            .context("could not determine a zuliprc path")?;
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("reading zuliprc at {}", path.display()))?;
        Self::parse(&contents)
    }
}

fn normalize_site(site: &str) -> anyhow::Result<String> {
    let site = site.trim();
    let site = if site.contains("://") {
        site.to_owned()
    } else {
        format!("https://{site}")
    };
    let url = reqwest::Url::parse(&site).context("invalid Zulip site URL")?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        anyhow::bail!("Zulip site must be an http or https origin");
    }
    Ok(url.origin().ascii_serialization())
}

#[cfg(test)]
mod tests {
    use super::Credentials;

    #[test]
    fn parses_normal_zuliprc() {
        let credentials = Credentials::parse(
            "[api]\nemail = me@example.com\nkey = secret\nsite = https://chat.example.com\n",
        )
        .unwrap();
        assert_eq!(credentials.email, "me@example.com");
        assert_eq!(credentials.key, "secret");
        assert_eq!(credentials.site, "https://chat.example.com");
    }

    #[test]
    fn rejects_missing_key() {
        let error =
            match Credentials::parse("[api]\nemail = me@example.com\nsite = chat.example.com\n") {
                Ok(_) => panic!("missing key must fail"),
                Err(error) => error,
            };
        assert!(error.to_string().contains("missing key"));
    }

    #[test]
    fn strips_site_trailing_slash() {
        let credentials =
            Credentials::parse("[api]\nemail=a\nkey=b\nsite=https://chat.example.com/\n").unwrap();
        assert_eq!(credentials.site, "https://chat.example.com");
    }

    #[test]
    fn adds_https_to_bare_hostname() {
        let credentials =
            Credentials::parse("[api]\nemail=a\nkey=b\nsite=chat.example.com\n").unwrap();
        assert_eq!(credentials.site, "https://chat.example.com");
    }

    #[test]
    fn ignores_comments_and_surrounding_whitespace() {
        let credentials = Credentials::parse("# leading comment\n [api] ; comments too\n email = a@example.com # not part of email\n key = k ; not part of key\n site = chat.example.com/\n").unwrap();
        assert_eq!(credentials.email, "a@example.com");
        assert_eq!(credentials.key, "k");
        assert_eq!(credentials.site, "https://chat.example.com");
    }
}
