//! Pages as the frame shows them.
//!
//! [`PageSystem`] is the one entity behind every page surface: it owns the
//! [`Browser`] (and so Chrome), the timers that close what nothing shows
//! ([`pages`]), and the `page` address kind. A page surface is the page
//! view itself; the system hears when one goes and starts that page's
//! grace period. The browser stays in this process: no host knows a page.
//!
//! A page takes every key, as a desktop does, so the frame's normal-mode
//! keys cannot reach the frame from one. Its chords can (`ctrl-k`,
//! `ctrl-j`, `ctrl-shift-f`, `ctrl-shift-backspace`), and the palette is
//! bound here on `ctrl-shift-;` (`ctrl-:` as gpui reads it), the chord a
//! desktop opens it with.

mod pages;

use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;

use gpui::{App, AppContext as _, Context, Entity, Subscription, TaskExt as _, Window};
use rho_dealer::{Address, Addressed};
use rho_frame::{
    AnySurface, Candidate, Found, Frame, FrameLink, Marks, Point, Row, Subject, Surface,
};
use rho_window::style::StyleClass;

use self::pages::{GRACE, Pages};
use url::Url;

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
    pages: Pages<gpui::Task<()>>,
    frame: FrameLink,
    /// The page model whose tab changes are heard, and the hearing.
    watched: Option<(Entity<PageModel>, Subscription)>,
}

impl PageSystem {
    /// The browser keeps its state under `state_dir`, the GUI's own state
    /// directory, and talks to its extension over a socket there: one
    /// GUI per state directory, which the browser's lock already says.
    pub fn new(state_dir: PathBuf, frame: FrameLink, cx: &mut Context<Self>) -> Self {
        bind_keys(cx);
        let socket = state_dir.join("rho-browser.sock");
        Self {
            browser: Browser::new(state_dir, socket),
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

    /// The surface over `page` went.
    fn hidden(&mut self, page: PageId, cx: &mut Context<Self>) {
        if self.pages.hide(page) {
            self.start_grace(page, cx);
        }
    }

    /// Opens a new tab at `url` and shows it.
    fn open_url(&mut self, url: &Url, cx: &mut Context<Self>) {
        let create = self.browser.create_page(url.to_string(), cx);
        if let Ok(model) = self.browser.model(cx) {
            self.watch(&model, cx);
        }
        cx.spawn(async move |this, cx| {
            let record = create.await;
            let _ = this.update(cx, |this, cx| match record {
                Ok(record) => this.frame.open(record.id.address(), cx),
                Err(error) => {
                    tracing::error!(%error, "browser page creation failed");
                    this.frame
                        .echo(format!("browser: {error:#}"), StyleClass::SystemInfo, cx);
                }
            });
        })
        .detach();
    }

    /// `page:` asks for a page: one the browser has open, by its title,
    /// most recent first, or a new one at what is typed.
    fn ask_page(this: Entity<Self>, frame: &mut Frame, window: &mut Window, cx: &mut Context<Frame>) {
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
                    Ok(url) => this.update(cx, |system, cx| system.open_url(&url, cx)),
                    Err(error) => frame.echo(
                        &format!("page: {input} is no web address: {error}"),
                        StyleClass::SystemInfo,
                        cx,
                    ),
                }
            }),
            window,
            cx,
        );
        if let Some(minibuffer) = frame.minibuffer_mut() {
            minibuffer.set_complete_whole_input();
        }
    }

}

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

/// What the user typed at `page:` as a web address: `https://` when they
/// named no scheme, and only the web's own schemes.
fn typed_url(input: &str) -> Result<Url, String> {
    let text = match input.contains("://") {
        true => input.to_owned(),
        false => format!("https://{input}"),
    };
    let url = Url::parse(&text).map_err(|error| error.to_string())?;
    match url.scheme() {
        "http" | "https" => Ok(url),
        scheme => Err(format!("{scheme} is not http or https")),
    }
}

impl rho_frame::System for PageSystem {
    fn kinds(&self) -> &'static [&'static str] {
        &[PageId::KIND]
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
                tracing::error!(%error, "the browser does not start");
                self.frame.echo(
                    format!("browser: {error:#}"),
                    StyleClass::SystemImportant,
                    cx,
                );
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

    fn rows(&mut self, _point: &Point, _window: &mut Window, cx: &mut Context<Self>) -> Vec<Row> {
        let this = cx.entity();
        vec![Row::run("o", "open page…", move |frame, window, cx| {
            Self::ask_page(this.clone(), frame, window, cx)
        })]
    }

    fn open_link(&mut self, url: &Url, cx: &mut Context<Self>) -> bool {
        self.open_url(url, cx);
        true
    }

    fn name(&self, address: &Address, _cx: &App) -> Option<(String, String)> {
        let page = PageId::from_address(address)?;
        Some((title(page), "web".to_owned()))
    }

    fn find(&self, _marks: &Marks, _cx: &App) -> Vec<Found> {
        live_pages()
            .into_iter()
            .map(|(id, metadata)| Found {
                address: id.address(),
                title: title(id),
                description: format!("web · {}", metadata.url),
                recency: metadata.last_accessed_ms,
            })
            .collect()
    }
}

impl Surface for PageView {
    fn title(&self, _cx: &App) -> String {
        title(self.page_id())
    }

    fn subject(&self, _cx: &App) -> Subject {
        Subject::Own
    }
}

fn bind_keys(cx: &mut App) {
    use gpui::KeyBinding;
    use rho_frame::keys::Palette;
    // Under the frame's own context, so the frame's handlers hear them;
    // a page takes every key that no binding names.
    let context = format!("{} > RhoBrowser", rho_frame::KEY_CONTEXT);
    cx.bind_keys([
        // `ctrl-shift-;` as gpui reads it on Linux: shift makes the key
        // `:` and is dropped, so `ctrl-shift-;` would never match.
        KeyBinding::new("ctrl-:", Palette, Some(&context)),
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;

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
        for other in ["ftp://example.com/", "file:///etc/passwd", "chrome://settings"] {
            assert!(typed_url(other).is_err(), "{other}");
        }
        // No `://`, so it reads as a host with a port-like tail, which is
        // no web address rather than script.
        assert!(typed_url("javascript:alert(1)").is_err());
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
