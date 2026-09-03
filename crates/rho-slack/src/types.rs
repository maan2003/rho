//! Wire identities and the shapes rho keeps from Slack's payloads.
//!
//! Slack's ids and message timestamps are opaque strings that identify but
//! never read: they are newtypes here so nothing formats one into a label by
//! accident. The only human-facing names are channel and user names, which
//! the model resolves.

use serde::{Deserialize, Serialize};

use crate::config::WorkspaceName;

macro_rules! opaque_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }
    };
}

opaque_id!(ChannelId, "A channel, group, or DM conversation id.");
opaque_id!(UserId, "A member of the workspace.");

/// A Slack message timestamp: both the message's id within its channel and
/// its send time. Never rendered — the surface shows a clock time derived
/// from it, and the dealer compares them.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Ts(pub String);

impl Ts {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Seconds since the epoch, for ordering and for the clock time shown
    /// next to a message. A malformed timestamp sorts oldest rather than
    /// panicking: a thread that renders slightly out of order beats a client
    /// that dies on one odd frame.
    pub fn epoch_seconds(&self) -> f64 {
        self.0.parse().unwrap_or(0.0)
    }

    pub fn millis(&self) -> i64 {
        (self.epoch_seconds() * 1000.0) as i64
    }

    /// Slack's own ordering: numeric, not lexicographic, because the integer
    /// part grows a digit every few years.
    pub fn is_newer_than(&self, other: &Self) -> bool {
        self.epoch_seconds() > other.epoch_seconds()
    }
}

impl From<&str> for Ts {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

/// The identity of one thread, everywhere in rho: the dealer card, the
/// surface, the desk node, and the journal all key on this.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ThreadKey {
    pub workspace: WorkspaceName,
    pub channel: ChannelId,
    /// The parent message of the thread. A mention that is not in a thread
    /// is its own thread root, which is exactly how a reply to it behaves.
    pub thread_ts: Ts,
}

/// Why a thread is rho's business at all. Channel traffic the user was not
/// addressed in never becomes an item, so this is a closed set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reason {
    /// The user was named, by handle, group, or a channel-wide broadcast.
    Mention,
    /// A direct message.
    DirectMessage,
    /// A reply in a thread the user has posted in.
    Thread,
}

/// Where a conversation lives, for the one line of chrome above a thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConversationKind {
    Channel,
    Group,
    DirectMessage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conversation {
    pub id: ChannelId,
    pub kind: ConversationKind,
    /// Slack's own name. For a channel this is what the user reads; for a
    /// group DM it is the machine name `mpdm-david--keith-1`, which is why
    /// the label a group DM shows is built by the model out of its members
    /// instead.
    pub name: String,
    /// The other person, for a one-to-one DM. Slack names a DM only by that
    /// id, so the label waits on the roster.
    pub user: Option<UserId>,
    /// Everyone in a group DM, when Slack sent them. Absent on the
    /// `users.conversations` payload, where the handles in `name` are all
    /// there is to go on.
    pub members: Vec<UserId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User {
    pub id: UserId,
    /// The display name, falling back to the handle Slack always provides.
    pub name: String,
    /// Slack's `name` field, the handle. A group DM's machine name is built
    /// out of handles, so this is what turns one back into people.
    pub handle: String,
}

/// One message as rho keeps it: who, when, the rendered text, and the two
/// timestamps that place it in its thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub ts: Ts,
    pub thread_ts: Option<Ts>,
    pub channel: ChannelId,
    pub user: Option<UserId>,
    /// A bot or app post has no user id; its name arrives inline.
    pub bot_name: Option<String>,
    /// Block Kit as received, kept unrendered so names can be resolved later
    /// when a user or channel first becomes known.
    pub blocks: Vec<serde_json::Value>,
    /// The plain `text` field, used when a message carries no blocks.
    pub text: String,
    pub attachments: Vec<Attachment>,
    pub files: Vec<FileSummary>,
    /// What the message *is* rather than what it says: `thread_broadcast`,
    /// `channel_join`, and the rest of Slack's subtypes.
    pub subtype: Option<String>,
    /// The thread hanging under this message, as Slack counts it. Only a
    /// parent carries these.
    pub reply_count: u32,
    pub latest_reply: Option<Ts>,
    /// Whether the author changed it after sending.
    pub edited: bool,
    pub reactions: Vec<Reaction>,
}

/// One emoji on a message, with who put it there: a reader wants to know
/// whether they already reacted, and that is the only reason the ids are
/// kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reaction {
    pub name: String,
    pub count: u32,
    pub users: Vec<UserId>,
}

impl Message {
    /// The thread this message belongs to: its parent, or itself when it is
    /// the parent.
    pub fn thread_root(&self) -> Ts {
        self.thread_ts.clone().unwrap_or_else(|| self.ts.clone())
    }

    /// A reply that Slack also put in the channel. It belongs in both
    /// places, unlike an ordinary reply, which belongs only in its thread.
    pub fn is_broadcast(&self) -> bool {
        self.subtype.as_deref() == Some("thread_broadcast")
    }

    /// Whether this message belongs on the channel surface: a top-level
    /// message, or a reply Slack broadcast there.
    pub fn is_top_level(&self) -> bool {
        match &self.thread_ts {
            None => true,
            Some(thread_ts) => *thread_ts == self.ts || self.is_broadcast(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Attachment {
    pub title: Option<String>,
    pub text: Option<String>,
    pub fallback: Option<String>,
    /// The line an app puts above its card.
    pub pretext: Option<String>,
    /// An app card's labelled values, in the order Slack sent them.
    pub fields: Vec<(String, String)>,
    /// A link preview rather than an app's own message. Slack paints the
    /// whole page; rho collapses it, because the reader asked for the
    /// conversation, not the web.
    pub is_unfurl: bool,
    /// Where the card points, which is what `enter` on it opens.
    pub url: Option<String>,
    /// The site Slack named for an unfurl, `github.com` and the like.
    pub service: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileSummary {
    pub id: String,
    pub title: String,
    /// Slack's short type name, `png` or `pdf`.
    pub filetype: String,
    pub size: u64,
    /// Where the bytes live. Reaching them needs the session's own token and
    /// cookie, like everything else here.
    pub url: String,
    /// The picture's own size, as Slack measured it. The box is drawn from
    /// this before any bytes arrive: a box that grows when they land pushes
    /// everything under it down the screen.
    pub original_w: u32,
    pub original_h: u32,
    /// Slack's smallest thumbnail. Blown up to fill the box it stands in for
    /// the picture while the real bytes are on their way: a blurhash with no
    /// encoding step, since the upscale is the blur.
    pub thumb_url: String,
}

impl FileSummary {
    /// `deck.pdf · pdf · 220 KB`: what it is and how big, which is all a
    /// reader needs to decide whether to open it.
    pub fn line(&self) -> String {
        let mut parts = vec![self.title.clone()];
        // `image.png · png` says it twice. The type earns its place only
        // when the name does not already carry it.
        let named = self
            .title
            .rsplit_once('.')
            .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case(&self.filetype));
        if !self.filetype.is_empty() && !named {
            parts.push(self.filetype.clone());
        }
        if self.size > 0 {
            parts.push(human_size(self.size));
        }
        parts.join(" · ")
    }

    /// The thumbnail as a file of its own, so it is fetched and cached by
    /// the one path that fetches and caches everything else.
    pub fn thumbnail(&self) -> Option<Self> {
        (!self.thumb_url.is_empty()).then(|| Self {
            id: format!("{}#thumb", self.id),
            title: format!("thumb-{}", self.title),
            size: 0,
            url: self.thumb_url.clone(),
            thumb_url: String::new(),
            ..self.clone()
        })
    }

    /// How many rows the picture's box takes: its own shape, capped at
    /// [`IMAGE_ROWS`]. A picture Slack never measured gets the cap, which is
    /// the box it would have had anyway.
    pub fn image_rows(&self) -> u32 {
        let (width, height) = (self.original_w, self.original_h);
        if width == 0 || height == 0 {
            return IMAGE_ROWS;
        }
        // The box is at most IMAGE_COLUMNS wide, and a monospace cell is
        // about half as wide as it is tall, so a picture that wide is this
        // many rows deep.
        let rows = IMAGE_COLUMNS as f32 * CELL_ASPECT * height as f32 / width as f32;
        (rows.ceil() as u32).clamp(1, IMAGE_ROWS)
    }

    pub fn is_image(&self) -> bool {
        matches!(
            self.filetype.as_str(),
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg"
        )
    }
}

/// How many lines tall an inline picture is allowed to be.
pub const IMAGE_ROWS: u32 = 12;

/// How wide, in columns. Together with the cap this is the box a picture is
/// fitted into, whatever its own size.
pub const IMAGE_COLUMNS: u32 = 48;

/// A monospace cell's width over its height, near enough for sizing a box.
pub const CELL_ASPECT: f32 = 0.5;

fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    match bytes {
        bytes if bytes >= MB => format!("{:.1} MB", bytes as f64 / MB as f64),
        bytes if bytes >= KB => format!("{} KB", bytes / KB),
        bytes => format!("{bytes} B"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(width: u32, height: u32) -> FileSummary {
        FileSummary {
            id: "F1".to_owned(),
            title: "image.png".to_owned(),
            filetype: "png".to_owned(),
            size: 225_280,
            url: "https://files.example/image.png".to_owned(),
            original_w: width,
            original_h: height,
            thumb_url: "https://files.example/thumb.png".to_owned(),
        }
    }

    #[test]
    fn a_pictures_box_is_its_own_shape_capped_at_twelve_rows() {
        // A tall picture takes the cap; a wide one takes what it needs, so
        // the box is never a hole the picture does not fill.
        assert_eq!(picture(400, 1200).image_rows(), IMAGE_ROWS);
        assert_eq!(picture(320, 200).image_rows(), IMAGE_ROWS);
        assert_eq!(picture(1200, 200).image_rows(), 4);
        assert_eq!(picture(2400, 100).image_rows(), 1);
        // Slack did not say, so the box is the cap it would have had.
        assert_eq!(picture(0, 0).image_rows(), IMAGE_ROWS);
    }

    #[test]
    fn a_thumbnail_is_a_file_of_its_own() {
        let thumb = picture(320, 200).thumbnail().expect("a thumbnail");
        assert_eq!(thumb.id, "F1#thumb");
        assert_eq!(thumb.url, "https://files.example/thumb.png");
        // The box it stands in is the picture's, not its own tiny size.
        assert_eq!(thumb.image_rows(), picture(320, 200).image_rows());
        assert!(
            FileSummary {
                thumb_url: String::new(),
                ..picture(320, 200)
            }
            .thumbnail()
            .is_none()
        );
    }
}
