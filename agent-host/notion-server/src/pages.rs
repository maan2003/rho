//! Which pages agents reach: the root page `rho notion init` names, and
//! the pages under it. Agents create pages there, and read, edit and
//! comment on them; the rest of the user's workspace stays out of reach.
//!
//! Notion MCP has no such scope, so the server checks each page a call
//! names. A page is under the root when `notion-fetch` lists the root in
//! its `<ancestor-path>`.

use axum::http::StatusCode;
use serde_json::{Map, Value, json};

use crate::{AppState, Failure, ROOT_PAGE, call_tool};

/// Runs one listed tool for an agent, within the root.
pub(crate) async fn call(
    state: &AppState,
    name: &str,
    mut arguments: Map<String, Value>,
) -> Result<Value, Failure> {
    let root = root(state)?;
    match name {
        "notion-fetch" => {
            let id = page_argument(&arguments, "id")?;
            let result = call_tool(state, name, arguments).await?;
            if id == root || is_cached(state, &id) || lists_ancestor(&result, &id, &root) {
                remember(state, id);
                Ok(result)
            } else {
                Err(outside())
            }
        }
        "notion-create-pages" => {
            if arguments.contains_key("creation_mode") {
                return Err(invalid("creation_mode: pages go under the root page"));
            }
            match arguments.get("parent") {
                None => {
                    arguments.insert("parent".to_owned(), json!({ "page_id": root }));
                }
                Some(Value::Object(parent))
                    if parent.keys().all(|key| key == "page_id" || key == "type") =>
                {
                    let id = page_argument(parent, "page_id")?;
                    require_within(state, &id, &root).await?;
                }
                Some(_) => return Err(invalid("parent: only a page_id under the root page")),
            }
            call_tool(state, name, arguments).await
        }
        "notion-update-page" => {
            if arguments.contains_key("template_id")
                || arguments.get("command").and_then(Value::as_str) == Some("apply_template")
            {
                // A template copies another page in.
                return Err(invalid("apply_template is not available"));
            }
            require_within(state, &page_argument(&arguments, "page_id")?, &root).await?;
            call_tool(state, name, arguments).await
        }
        "notion-create-comment" | "notion-get-comments" => {
            require_within(state, &page_argument(&arguments, "page_id")?, &root).await?;
            if let Some(discussion) = arguments.get("discussion_id") {
                // discussion://<page>/<block>/<discussion>
                let page = discussion
                    .as_str()
                    .and_then(|url| url.strip_prefix("discussion://"))
                    .and_then(|path| path.split('/').next())
                    .and_then(page_id)
                    .ok_or_else(|| invalid("discussion_id: pass its discussion:// URL"))?;
                require_within(state, &page, &root).await?;
            }
            call_tool(state, name, arguments).await
        }
        _ => call_tool(state, name, arguments).await,
    }
}

fn root(state: &AppState) -> Result<String, Failure> {
    (state.read_secret)(ROOT_PAGE)
        .ok()
        .as_deref()
        .and_then(page_id)
        .ok_or_else(|| {
            Failure::Refused(
                StatusCode::SERVICE_UNAVAILABLE,
                "rho_no_notion_root: run `rho notion init` on the agent host".to_owned(),
            )
        })
}

async fn require_within(state: &AppState, id: &str, root: &str) -> Result<(), Failure> {
    if id == root || is_cached(state, id) {
        return Ok(());
    }
    let mut arguments = Map::new();
    arguments.insert("id".to_owned(), json!(id));
    let fetched = call_tool(state, "notion-fetch", arguments).await?;
    if lists_ancestor(&fetched, id, root) {
        remember(state, id.to_owned());
        Ok(())
    } else {
        Err(outside())
    }
}

fn is_cached(state: &AppState, id: &str) -> bool {
    state.within_root.lock().expect("lock").contains(id)
}

fn remember(state: &AppState, id: String) {
    state.within_root.lock().expect("lock").insert(id);
}

/// Whether `fetched`, `notion-fetch`'s result for page `id`, lists `root`
/// among its ancestors.
fn lists_ancestor(fetched: &Value, id: &str, root: &str) -> bool {
    let text: String = fetched["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|part| part["text"].as_str())
        .collect();
    // The page arrives as JSON with its Markdown in `text`.
    let markdown = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|reply| reply["text"].as_str().map(str::to_owned))
        .unwrap_or(text);
    // The ancestors come right after the page's opening tag; anything later
    // is page content, which anyone with edit access writes.
    let Some((_, after)) = markdown.split_once("<page url=\"") else {
        return false;
    };
    let Some((url, after)) = after.split_once("\">") else {
        return false;
    };
    if page_id(url).as_deref() != Some(id) {
        return false;
    }
    let Some(path) = after.trim_start().strip_prefix("<ancestor-path>") else {
        return false;
    };
    let Some((path, _)) = path.split_once("</ancestor-path>") else {
        return false;
    };
    path.split("url=\"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .filter(|url| url.starts_with("https://"))
        .any(|url| page_id(url).as_deref() == Some(root))
}

/// A page's ID from an ID with or without dashes, or a page URL; `None`
/// for anything else, such as `collection://` or `memory`.
pub fn page_id(reference: &str) -> Option<String> {
    let reference = reference.trim();
    let candidate = if reference.starts_with("https://") {
        let path = reference.split(['?', '#']).next()?;
        // The ID ends the last path segment, after any title slug.
        let segment = path.trim_end_matches('/').rsplit('/').next()?;
        segment.get(segment.len().checked_sub(32)?..)?.to_owned()
    } else {
        reference.replace('-', "")
    };
    (candidate.len() == 32 && candidate.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| candidate.to_ascii_lowercase())
}

fn page_argument(arguments: &Map<String, Value>, name: &str) -> Result<String, Failure> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .and_then(page_id)
        .ok_or_else(|| invalid(&format!("{name}: a page ID or URL")))
}

fn invalid(detail: &str) -> Failure {
    Failure::Refused(
        StatusCode::BAD_REQUEST,
        format!("rho_invalid_arguments: {detail}"),
    )
}

fn outside() -> Failure {
    Failure::Refused(
        StatusCode::FORBIDDEN,
        "rho_page_outside_root: agents reach only the agent pages root and the pages under it"
            .to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_ids_come_from_ids_and_page_urls_only() {
        let id = "392eb0892aa08112bb4dc43774fe994d";
        for reference in [
            id,
            "392EB089-2AA0-8112-BB4D-C43774FE994D",
            "https://app.notion.com/p/392eb0892aa08112bb4dc43774fe994d?pvs=204",
            "https://www.notion.so/team/Fix-bad-392eb0892aa08112bb4dc43774fe994d#abc",
        ] {
            assert_eq!(page_id(reference).as_deref(), Some(id), "{reference}");
        }
        for reference in [
            "memory",
            "collection://2d8eb089-2aa0-8199-8451-000b4f7f1a21",
            "392eb0892aa08112bb4dc43774fe994",
            "https://app.notion.com/p/short",
        ] {
            assert_eq!(page_id(reference), None, "{reference}");
        }
    }
}
