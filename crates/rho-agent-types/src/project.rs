//! The settings owned by a project on its host.

use bytes::{Buf, BytesMut};
use senax_encoder::{Decode, Decoder, Encode, Encoder, Pack, Packer, Unpack, Unpacker};

use crate::{MessageId, one_line};

/// A project, named by its anchor: the root of its host's chat that a
/// [`ChatEvent::Project`](crate::ChatEvent::Project) made a project.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub struct ProjectId(pub MessageId);

/// A repository address or a directory on the host, as the user wrote it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct ProjectPlace(String);

impl ProjectPlace {
    pub fn parse(value: &str) -> Result<Self, &'static str> {
        let value = value.trim();
        if value.is_empty() {
            return Err("enter a repository address or directory");
        }
        if value.chars().any(char::is_control) {
            return Err("the place must be one line");
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_repository(&self) -> bool {
        self.0.starts_with("git@")
            || self.0.split_once("://").is_some_and(|(scheme, _)| {
                !scheme.is_empty()
                    && scheme
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c))
            })
    }

    /// The place's short name in the Desk's project heading.
    pub fn label(&self) -> &str {
        let place = self.0.trim_end_matches('/');
        let tail = if self.is_repository() {
            place.rsplit(['/', ':']).next().unwrap_or(place)
        } else {
            place.rsplit('/').next().unwrap_or(place)
        };
        if tail.is_empty() {
            self.as_str()
        } else if self.is_repository() {
            tail.strip_suffix(".git").unwrap_or(tail)
        } else {
            tail
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum ProjectMode {
    One,
    PerIdea,
}

impl ProjectMode {
    pub const ALL: [Self; 2] = [Self::One, Self::PerIdea];

    pub fn label(self) -> &'static str {
        match self {
            Self::One => "one",
            Self::PerIdea => "per idea",
        }
    }
}

/// Which runtime an agent runs on (and whose transcript it produces).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Encode, Decode, Pack, Unpack)]
pub enum RuntimeKind {
    Rho,
    Claude,
}

/// A validated label for the model and effort the user or project chose.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RuntimeLabel(String);

/// The models each runtime kind offers on this build's host, and the
/// efforts each takes, as `_rho.pyi` lists them. A client never reads these:
/// it offers what its host says in [`RuntimeCatalogue`].
const NATIVE_MODELS: [&str; 5] = ["sol", "terra", "astra", "hydrogen", "neon"];
const NATIVE_EFFORTS: [&str; 4] = ["low", "medium", "high", "xhigh"];
const CLAUDE_MODELS: [&str; 4] = ["fable", "opus", "sonnet", "haiku"];
const CLAUDE_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// What the model picker cycles through, first the default. Any other
/// pair the policy takes can be typed.
///
/// The default is the runtime the user picks for agents they start from a
/// message: `opus-high`, decided once here rather than learned, so a start
/// that asks nothing still runs where the user would have put it.
pub const RUNTIMES: [&str; 8] = [
    "opus-high",
    "sol-medium",
    "sol-high",
    "terra-medium",
    "astra-high",
    "hydrogen-high",
    "neon-high",
    "fable-high",
];

pub const DEFAULT_RUNTIME: &str = RUNTIMES[0];

impl RuntimeLabel {
    /// A `<model>-<effort>` as it crossed the wire: its shape is checked,
    /// not whether this build knows it, so a newer host's runtime still
    /// reads. Whether a host offers it is its catalogue's answer.
    fn read(text: &str) -> Result<Self, String> {
        let text = text.trim().to_ascii_lowercase();
        let part = |part: &str| !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric());
        match text.split_once('-') {
            Some((model, effort)) if part(model) && part(effort) => Ok(Self(text)),
            _ => Err(format!("`{text}` is not <model>-<effort>")),
        }
    }

    /// Reads a supported `<model>-<effort>`; a missing label is not a label.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim().to_ascii_lowercase();
        let refusal = || {
            format!(
                "unknown model `{text}`; write <model>-<effort>, like {}",
                RUNTIMES.join(", ")
            )
        };
        let (model, effort) = text.split_once('-').ok_or_else(refusal)?;
        if (NATIVE_MODELS.contains(&model) && NATIVE_EFFORTS.contains(&effort))
            || (CLAUDE_MODELS.contains(&model) && CLAUDE_EFFORTS.contains(&effort))
        {
            Ok(Self(text))
        } else {
            Err(refusal())
        }
    }

    /// All model/effort pairs the host supports, in catalogue order.
    pub fn choices(kind: RuntimeKind) -> impl Iterator<Item = String> {
        let (models, efforts) = match kind {
            RuntimeKind::Rho => (NATIVE_MODELS.as_slice(), NATIVE_EFFORTS.as_slice()),
            RuntimeKind::Claude => (CLAUDE_MODELS.as_slice(), CLAUDE_EFFORTS.as_slice()),
        };
        models.iter().flat_map(move |model| {
            efforts
                .iter()
                .map(move |effort| format!("{model}-{effort}"))
        })
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn label(&self) -> String {
        self.0.clone()
    }

    pub fn kind(&self) -> RuntimeKind {
        if NATIVE_MODELS.contains(&self.model()) {
            RuntimeKind::Rho
        } else {
            RuntimeKind::Claude
        }
    }

    fn parts(&self) -> (&str, &str) {
        self.0.split_once('-').expect("validated runtime label")
    }

    pub fn model(&self) -> &str {
        self.parts().0
    }

    pub fn effort(&self) -> &str {
        self.parts().1
    }
}

impl Default for RuntimeLabel {
    fn default() -> Self {
        Self::parse(DEFAULT_RUNTIME).expect("the default is in the runtime catalogue")
    }
}

impl std::fmt::Display for RuntimeLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Encoder for RuntimeLabel {
    fn encode(&self, writer: &mut BytesMut) -> senax_encoder::Result<()> {
        self.0.encode(writer)
    }

    fn is_default(&self) -> bool {
        self.as_str() == DEFAULT_RUNTIME
    }
}

impl Decoder for RuntimeLabel {
    fn decode(reader: &mut impl Buf) -> senax_encoder::Result<Self> {
        Self::read(&String::decode(reader)?).map_err(senax_encoder::EncoderError::Decode)
    }
}

impl Packer for RuntimeLabel {
    fn pack(&self, writer: &mut BytesMut) -> senax_encoder::Result<()> {
        self.0.pack(writer)
    }
}

impl Unpacker for RuntimeLabel {
    fn unpack(reader: &mut impl Buf) -> senax_encoder::Result<Self> {
        Self::read(&String::unpack(reader)?).map_err(senax_encoder::EncoderError::Decode)
    }
}

/// One runtime a host offers.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct RuntimeChoice {
    pub label: RuntimeLabel,
    pub kind: RuntimeKind,
    /// Offered first, before every other pair of its kind.
    pub usual: bool,
}

/// The runtimes a host offers, in its order: the usual ones first, its
/// default first of all. The host says it to every client in its chat
/// session's `Hello`, so a client offers and accepts the host's runtimes,
/// not the ones it was built with.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct RuntimeCatalogue {
    pub choices: Vec<RuntimeChoice>,
}

impl RuntimeCatalogue {
    /// What this build's host offers.
    pub fn host() -> Self {
        let usual = RUNTIMES.iter().map(|label| RuntimeChoice {
            label: RuntimeLabel::parse(label).expect("a usual runtime is supported"),
            kind: RuntimeLabel::parse(label).expect("supported").kind(),
            usual: true,
        });
        let rest = [RuntimeKind::Rho, RuntimeKind::Claude]
            .into_iter()
            .flat_map(|kind| {
                RuntimeLabel::choices(kind)
                    .filter(|label| !RUNTIMES.contains(&label.as_str()))
                    .map(move |label| RuntimeChoice {
                        label: RuntimeLabel(label),
                        kind,
                        usual: false,
                    })
            });
        Self {
            choices: usual.chain(rest).collect(),
        }
    }

    /// The host's default: its first runtime.
    pub fn default_runtime(&self) -> Option<&RuntimeLabel> {
        self.choices.first().map(|choice| &choice.label)
    }

    /// What the user typed, if the host offers it.
    pub fn label(&self, text: &str) -> Result<RuntimeLabel, String> {
        let label = RuntimeLabel::read(text)?;
        self.choice(&label)
            .map(|choice| choice.label.clone())
            .ok_or_else(|| {
                let usual: Vec<_> = self
                    .choices
                    .iter()
                    .filter(|choice| choice.usual)
                    .map(|choice| choice.label.as_str())
                    .collect();
                format!(
                    "the host has no model `{label}`; write <model>-<effort>, like {}",
                    usual.join(", ")
                )
            })
    }

    pub fn choice(&self, label: &RuntimeLabel) -> Option<&RuntimeChoice> {
        self.choices.iter().find(|choice| choice.label == *label)
    }

    pub fn kind(&self, label: &RuntimeLabel) -> Option<RuntimeKind> {
        self.choice(label).map(|choice| choice.kind)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct ProjectConfig {
    pub place: ProjectPlace,
    pub mode: ProjectMode,
    pub runtime: RuntimeLabel,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum ProjectChange {
    Place(ProjectPlace),
    Mode(ProjectMode),
    Runtime(RuntimeLabel),
}

/// A validated name without the client-owned host alias.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct ProjectName(String);

impl ProjectName {
    pub fn parse(value: &str) -> Result<Self, &'static str> {
        let name = one_line(value);
        if name.is_empty() || name.contains(['@', '|']) {
            return Err("enter a project name without @ or |");
        }
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_runtime_label_validates_user_input_and_packed_data() {
        let sol = RuntimeLabel::parse(" SOL-xhigh ").unwrap();
        assert_eq!((sol.as_str(), sol.kind()), ("sol-xhigh", RuntimeKind::Rho));
        for invalid in ["", "sol-max", "astra", "unknown-high"] {
            assert!(RuntimeLabel::parse(invalid).is_err());
        }
        for malformed in ["", "astra", "sol-"] {
            let packed = senax_encoder::pack(&malformed.to_owned()).unwrap();
            assert!(senax_encoder::unpack::<RuntimeLabel>(&mut packed.as_ref()).is_err());
        }
        assert_eq!(RuntimeLabel::default().as_str(), "opus-high");
    }

    /// A client reads a runtime its own build does not know, and asks the
    /// host's catalogue whether it is offered.
    #[test]
    fn a_runtime_the_client_does_not_know_still_reads_and_the_hosts_catalogue_decides() {
        let packed = senax_encoder::pack(&"nova-high".to_owned()).unwrap();
        let newer = senax_encoder::unpack::<RuntimeLabel>(&mut packed.as_ref()).unwrap();
        assert_eq!(newer.as_str(), "nova-high");
        assert!(RuntimeLabel::parse("nova-high").is_err());
        let packed = senax_encoder::pack(&"no label".to_owned()).unwrap();
        assert!(senax_encoder::unpack::<RuntimeLabel>(&mut packed.as_ref()).is_err());

        let mut catalogue = RuntimeCatalogue::host();
        assert_eq!(catalogue.default_runtime().unwrap().as_str(), "opus-high");
        assert!(catalogue.label("nova-high").is_err());
        assert_eq!(catalogue.label(" SOL-low ").unwrap().as_str(), "sol-low");
        catalogue.choices.push(RuntimeChoice {
            label: newer.clone(),
            kind: RuntimeKind::Rho,
            usual: false,
        });
        assert_eq!(catalogue.label("nova-high"), Ok(newer.clone()));
        assert_eq!(catalogue.kind(&newer), Some(RuntimeKind::Rho));
        let usual = catalogue
            .choices
            .iter()
            .take_while(|choice| choice.usual)
            .count();
        assert_eq!(usual, RUNTIMES.len());
    }

    #[test]
    fn a_directory_can_have_a_git_suffix_or_colon_without_becoming_a_repository() {
        for (input, repository, label) in [
            ("git@github.com:owner/tide.git", true, "tide"),
            ("ssh://box:2222/owner/strand.git/", true, "strand"),
            ("/src/box:reef.git/", false, "box:reef.git"),
            ("/", false, "/"),
        ] {
            let place = ProjectPlace::parse(input).unwrap();
            assert_eq!(place.is_repository(), repository, "{input}");
            assert_eq!(place.label(), label, "{input}");
        }
        assert!(ProjectPlace::parse("/one\n/two").is_err());
    }
}
