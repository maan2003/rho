//! Pages as the frame shows them.
//!
//! [`PageSystem`] is the one entity behind every page surface: it owns the
//! [`Browser`] (and so Chrome), the timers that close tabs no screen ever
//! showed ([`pages`]), and the `page` address kind. A page surface is the
//! page view itself; the system hears when one goes, which is the user
//! closing it, and closes its tab. The browser stays in this process: no
//! host knows a page.
//!
//! The site takes the keys, except for the window's reserved chords.
//! `Surface::rows` makes the window's navigation discoverable from the
//! palette, with shortcuts resolved in the page's actual key context.

mod bookmarks;
mod pages;

use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;

use gpui::{App, AppContext as _, Context, Entity, Subscription, TaskExt as _, Window};
use rho_db::RhoDb;
use rho_dealer::{Address, Addressed};
use rho_frame::{AnySurface, Candidate, Found, Frame, FrameLink, Marks, Point, Row, Surface, keys};
use rho_window::style::StyleClass;
use url::Url;

use self::bookmarks::BookmarkId;
use self::pages::{GRACE, Pages};
use crate::runtime::{CannotDraw, NoBrowser};
use crate::{
    Browser, PageId, PageMetadata, PageMetadataChanged, PageModel, PageView, live_page_name,
    live_pages,
};

/// `page:<uuid>`: the page id the extension minted, as it says it
/// (`SPEC-page-addresses`).
impl Addressed for PageId {
    const KIND: &'static str = "page";

    fn rest(&self) -> String {
        self.0.hyphenated().to_string()
    }

    fn from_rest(rest: &str) -> Option<Self> {
        let id = uuid::Uuid::try_parse(rest).ok()?;
        // One page, one text: two addresses are the same thing only when
        // their text is.
        (id.hyphenated().to_string() == rest).then_some(Self(id))
    }
}

pub struct PageSystem {
    browser: Browser,
    db: RhoDb,
    pages: Pages<gpui::Task<()>>,
    frame: FrameLink,
    /// The page model whose tab changes are heard, and the hearing.
    watched: Option<(Entity<PageModel>, Subscription)>,
}

impl PageSystem {
    /// The browser keeps its state under `state_dir`, the GUI's own state
    /// directory, and talks to its extension over a socket there: one
    /// GUI per state directory, which the browser's lock already says.
    pub fn new(state_dir: PathBuf, db: RhoDb, frame: FrameLink) -> Self {
        let socket = state_dir.join("rho-browser.sock");
        Self {
            browser: Browser::new(state_dir, socket),
            db,
            pages: Pages::default(),
            frame,
            watched: None,
        }
    }

    /// Hears the tabs of `model`, the browser's page model, once per model:
    /// a browser that died and was launched again has a new one.
    fn watch(&mut self, model: &Entity<PageModel>, cx: &mut Context<Self>) {
        if self
            .watched
            .as_ref()
            .is_some_and(|(watched, _)| watched == model)
        {
            return;
        }
        let subscription = cx.subscribe(model, |this, _, _: &PageMetadataChanged, cx| {
            this.tabs_moved(cx)
        });
        self.watched = Some((model.clone(), subscription));
        self.tabs_moved(cx);
    }

    /// The browser's tabs changed: what nothing shows starts its grace
    /// period, and a surface whose tab is gone goes with it.
    fn tabs_moved(&mut self, cx: &mut Context<Self>) {
        let live: HashSet<PageId> = live_pages().into_iter().map(|(id, _)| id).collect();
        let moved = self.pages.live(live);
        for page in moved.start {
            self.start_grace(page, cx);
        }
        for page in moved.gone {
            self.frame.close(page.address(), cx);
        }
        cx.notify();
    }

    fn start_grace(&mut self, page: PageId, cx: &mut Context<Self>) {
        tracing::info!(page_id = %page, after = ?GRACE, "closing a page nothing shows");
        let timer = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(GRACE).await;
            let _ = this.update(cx, |this, cx| {
                if !this.pages.fired(page) {
                    return;
                }
                tracing::info!(page_id = %page, "closing a page nothing showed for its grace period");
                if let Some(close) = this.browser.close_page_if_running(page, cx) {
                    close.detach_and_log_err(cx);
                }
            });
        });
        self.pages.closing(page, timer);
    }

    /// The surface over `page` went: the user closed the page, so its tab
    /// closes with it.
    fn hidden(&mut self, page: PageId, cx: &mut Context<Self>) {
        if self.pages.hide(page)
            && let Some(close) = self.browser.close_page_if_running(page, cx)
        {
            close.detach_and_log_err(cx);
        }
    }

    /// Opens a new tab at `url` and shows it.
    fn open_url(&mut self, url: &Url, cx: &mut Context<Self>) {
        match self.browser.model(cx) {
            Ok(model) => self.watch(&model, cx),
            Err(error) => return self.cannot_open(error, cx),
        }
        let create = self.browser.create_page(url.to_string(), cx);
        cx.spawn(async move |this, cx| {
            let record = create.await;
            let _ = this.update(cx, |this, cx| match record {
                Ok(record) => this.frame.open(record.id.address(), cx),
                Err(error) => this.cannot_open(error, cx),
            });
        })
        .detach();
    }

    /// The echo answers in the user's words; diagnostics go to the log.
    fn cannot_open(&self, error: anyhow::Error, cx: &mut Context<Self>) {
        tracing::error!(error = format!("{error:#}"), "a page does not open");
        self.frame.echo(
            page_open_echo(&error).to_owned(),
            StyleClass::SystemImportant,
            cx,
        );
    }

    fn bookmark_page(
        this: Entity<Self>,
        url: Url,
        title: String,
        frame: &mut Frame,
        window: &mut Window,
        cx: &mut Context<Frame>,
    ) {
        let proposed = title.clone();
        frame.open_prompt(
            "bookmark as:",
            Rc::new(move |_, typed, _| {
                typed
                    .trim()
                    .is_empty()
                    .then(|| Candidate {
                        value: proposed.clone(),
                        description: String::new(),
                    })
                    .into_iter()
                    .collect()
            }),
            Rc::new(move |_, input, _, cx| {
                let title = input.split_whitespace().collect::<Vec<_>>().join(" ");
                if title.is_empty() {
                    return;
                }
                this.update(cx, |system, cx| {
                    let db = system.db.clone();
                    let url = url.clone();
                    let frame = system.frame.clone();
                    cx.spawn(async move |_, cx| {
                        let title = cx
                            .background_spawn(async move {
                                bookmarks::save(&db, &title, &url).await;
                                title
                            })
                            .await;
                        let _ = cx.update(|cx| {
                            frame.echo(format!("bookmarked: {title}"), StyleClass::SystemInfo, cx);
                        });
                    })
                    .detach();
                });
            }),
            window,
            cx,
        );
    }

    /// `page:` asks for a page: one the browser has open, by its title,
    /// most recent first, or a new one at what is typed.
    fn ask_page(
        this: Entity<Self>,
        frame: &mut Frame,
        window: &mut Window,
        cx: &mut Context<Frame>,
    ) {
        frame.open_prompt(
            "page:",
            Rc::new(|_, input, _| {
                let needle = input.trim().to_lowercase();
                recent_pages()
                    .into_iter()
                    .filter(|(page, metadata)| {
                        title(*page).to_lowercase().contains(&needle)
                            || metadata.url.to_lowercase().contains(&needle)
                    })
                    .map(|(page, metadata)| Candidate {
                        value: title(page),
                        description: metadata.url,
                    })
                    .collect()
            }),
            Rc::new(move |frame, input, window, cx| {
                let input = input.trim();
                if input.is_empty() {
                    return;
                }
                let open = recent_pages()
                    .into_iter()
                    .find(|(page, _)| title(*page) == input);
                if let Some((page, _)) = open {
                    return frame.open(page.address(), window, cx);
                }
                match typed_url(input) {
                    Some(url) => this.update(cx, |system, cx| system.open_url(&url, cx)),
                    None => frame.echo("not a web address", StyleClass::SystemInfo, cx),
                }
            }),
            window,
            cx,
        );
    }
}

fn page_open_echo(error: &anyhow::Error) -> &'static str {
    if error.is::<CannotDraw>() {
        "this display cannot draw a page"
    } else if error.is::<NoBrowser>() {
        "page: no browser found"
    } else {
        "page did not open"
    }
}

/// Where a page is, before its title.
const PLACE: &str = "web";

/// What a page is called: its title while the browser has it, or its
/// host, or nothing better than "page" before the browser has said.
fn title(page: PageId) -> String {
    live_page_name(page).unwrap_or_else(|| "page".to_owned())
}

/// Every page the browser has open, the one the reader was on last first.
fn recent_pages() -> Vec<(PageId, PageMetadata)> {
    let mut pages = live_pages();
    pages.sort_by_key(|(page, metadata)| (std::cmp::Reverse(metadata.last_accessed_ms), *page));
    pages
}

/// What the user typed at `page:` as a web address. With no scheme named,
/// `https://`, except for a host that is this machine or has no dot in its
/// name (`127.0.0.1:8080`, `localhost:3000`, `intranet/wiki`): those are
/// local servers, which rarely speak TLS, so `http://`. Only the web's own
/// schemes open.
fn typed_url(input: &str) -> Option<Url> {
    if input.contains("://") {
        let url = Url::parse(input).ok()?;
        return matches!(url.scheme(), "http" | "https").then_some(url);
    }
    let mut url = Url::parse(&format!("https://{input}")).ok()?;
    let local = match url.host() {
        Some(url::Host::Domain(name)) => !name.contains('.'),
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    };
    if local {
        url.set_scheme("http").expect("https becomes http");
    }
    Some(url)
}

impl rho_frame::System for PageSystem {
    fn kinds(&self) -> &'static [&'static str] {
        &[PageId::KIND, BookmarkId::KIND]
    }

    fn open(
        &mut self,
        address: &Address,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnySurface> {
        let page = PageId::from_address(address)?;
        let model = match self.browser.open_page(page, cx) {
            Ok(model) => model,
            Err(error) => {
                self.cannot_open(error, cx);
                return None;
            }
        };
        self.watch(&model, cx);
        let view = cx.new(|cx| PageView::new(model, page, cx));
        cx.observe_release(&view, move |this, _, cx| this.hidden(page, cx))
            .detach();
        self.pages.show(page);
        Some(AnySurface::new(view))
    }

    fn visit(&mut self, address: &Address, cx: &mut Context<Self>) -> bool {
        let Some(bookmark) =
            BookmarkId::from_address(address).and_then(|id| bookmarks::get(&self.db, id))
        else {
            return false;
        };
        if let Some((page, _)) = recent_pages()
            .into_iter()
            .find(|(_, page)| page.url == bookmark.url.as_str())
        {
            self.frame.open(page.address(), cx);
        } else {
            self.open_url(&bookmark.url, cx);
        }
        true
    }

    fn rows(&mut self, point: &Point, _window: &mut Window, cx: &mut Context<Self>) -> Vec<Row> {
        let this = cx.entity();
        let open = this.clone();
        let mut rows = vec![Row::run("o", "open page…", move |frame, window, cx| {
            Self::ask_page(open.clone(), frame, window, cx)
        })];
        if let Some(page) = PageId::from_address(&point.surface)
            && let Some(meta) = crate::native_host::page_metadata(page)
            && let Some(url) = bookmarks::page_url(&meta.url)
        {
            let title = title(page);
            rows.push(Row::run(
                "",
                "bookmark this page…",
                move |frame, window, cx| {
                    Self::bookmark_page(this.clone(), url.clone(), title.clone(), frame, window, cx)
                },
            ));
        }
        rows
    }

    fn open_link(&mut self, url: &Url, cx: &mut Context<Self>) -> bool {
        self.open_url(url, cx);
        true
    }

    fn name(
        &self,
        address: &Address,
        _marks: &rho_dealer::Marks,
        _cx: &App,
    ) -> Option<(String, String)> {
        let page = PageId::from_address(address)?;
        Some((title(page), PLACE.to_owned()))
    }

    fn find(&self, _marks: &Marks, _cx: &App) -> Vec<Found> {
        live_pages()
            .into_iter()
            .map(|(id, metadata)| Found {
                address: id.address(),
                title: title(id),
                description: format!("web · {}", metadata.url),
                thread: None,
                recency: metadata.last_accessed_ms,
            })
            .chain(
                bookmarks::named(&self.db)
                    .into_iter()
                    .map(|bookmark| Found {
                        address: bookmark.id.address(),
                        title: bookmark.title,
                        description: format!(
                            "bookmark · {}",
                            bookmark.url.host_str().unwrap_or_default()
                        ),
                        thread: None,
                        recency: 0,
                    }),
            )
            .collect()
    }
}

impl Surface for PageView {
    fn title(&self, _cx: &App) -> String {
        format!("{PLACE} › {}", title(self.page_id()))
    }

    fn rows(
        &mut self,
        _: Option<rho_frame::surface::Seen>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Vec<Row> {
        vec![
            Row::run("", "go back", |frame, window, cx| frame.back(window, cx))
                .bound_to(keys::Back),
            Row::run("", "go forward", |frame, window, cx| {
                frame.forward_or_deal(window, cx)
            })
            .bound_to(keys::Forward),
            Row::run("", "close page", |frame, window, cx| {
                frame.close_current(window, cx)
            })
            .bound_to(keys::Close),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_open_echo_does_not_include_diagnostics() {
        let display = anyhow::anyhow!("failed to bind /run/user/123/rho-chrome-1")
            .context(CannotDraw)
            .context("launch browser");
        assert_eq!(page_open_echo(&display), "this display cannot draw a page");
        let other = anyhow::anyhow!("extension replied with internal error");
        assert_eq!(page_open_echo(&other), "page did not open");
    }

    #[test]
    fn a_page_address_is_the_extensions_id_in_one_spelling() {
        let id = PageId(uuid::Uuid::parse_str("018fbe9a-4e55-7b7d-a57e-7f7f4f31d1cd").unwrap());
        let address = id.address();
        assert_eq!(address.key(), "page:018fbe9a-4e55-7b7d-a57e-7f7f4f31d1cd");
        assert_eq!(
            Address::parse(address.key())
                .as_ref()
                .and_then(PageId::from_address),
            Some(id)
        );
        // Other spellings of the same UUID would be other addresses of the
        // same page, so none of them reads.
        for other in [
            "web-018fbe9a-4e55-7b7d-a57e-7f7f4f31d1cd",
            "018FBE9A-4E55-7B7D-A57E-7F7F4F31D1CD",
            "018fbe9a4e557b7da57e7f7f4f31d1cd",
            "{018fbe9a-4e55-7b7d-a57e-7f7f4f31d1cd}",
        ] {
            assert_eq!(
                PageId::from_address(&Address::new("page", other)),
                None,
                "{other}"
            );
        }
        assert_eq!(
            PageId::from_address(&Address::new("thread", id.rest())),
            None
        );
    }

    #[test]
    fn a_typed_address_without_a_scheme_is_https_and_only_the_web_opens() {
        assert_eq!(
            typed_url("example.com/a?b=1").unwrap().as_str(),
            "https://example.com/a?b=1"
        );
        // A scheme the user named is kept, not upgraded.
        assert_eq!(
            typed_url("http://127.0.0.1:8765/").unwrap().as_str(),
            "http://127.0.0.1:8765/"
        );
        for other in [
            "ftp://example.com/",
            "file:///etc/passwd",
            "chrome://settings",
        ] {
            assert!(typed_url(other).is_none(), "{other}");
        }
        // No `://`, so it reads as a host with a port-like tail, which is
        // no web address rather than script.
        assert!(typed_url("javascript:alert(1)").is_none());
    }

    #[test]
    fn a_typed_local_server_is_plain_http() {
        for (typed, opened) in [
            (
                "127.0.0.1:18080/index.html",
                "http://127.0.0.1:18080/index.html",
            ),
            ("localhost:3000", "http://localhost:3000/"),
            ("[::1]:8080/a", "http://[::1]:8080/a"),
            ("intranet/wiki", "http://intranet/wiki"),
            // A dotted name or an address off this machine stays https.
            ("example.com:8443/x", "https://example.com:8443/x"),
            ("10.0.0.5:8080", "https://10.0.0.5:8080/"),
            ("127.example.com", "https://127.example.com/"),
            // A scheme the user named is kept either way.
            ("https://localhost:3000/", "https://localhost:3000/"),
        ] {
            assert_eq!(typed_url(typed).unwrap().as_str(), opened, "{typed}");
        }
    }

    #[test]
    fn recent_pages_put_the_one_last_visited_first() {
        let ids = [
            "00000000-0000-4000-8000-000000000201",
            "00000000-0000-4000-8000-000000000202",
            "00000000-0000-4000-8000-000000000203",
        ];
        // The page with the smallest id was visited longest ago, so an
        // order by id and an order by visit disagree.
        for (id, last_accessed_ms) in ids.iter().zip([100u64, 300, 200]) {
            crate::native_host::record_page_metadata(&serde_json::json!({
                "event": "page-metadata",
                "page_id": id,
                "title": id,
                "url": "https://example.com/",
                "last_accessed_ms": last_accessed_ms,
            }));
        }
        let ids: Vec<PageId> = ids.iter().map(|id| id.parse().unwrap()).collect();
        let order: Vec<PageId> = recent_pages()
            .into_iter()
            .map(|(page, _)| page)
            .filter(|page| ids.contains(page))
            .collect();
        assert_eq!(order, vec![ids[1], ids[2], ids[0]]);
    }
}
