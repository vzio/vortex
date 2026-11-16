//! The instant skeleton of a file, from its `tags.scm` query: shown while its document symbols
//! load, and kept when the server has none. Definitions are nested by their ranges (a trait's
//! functions under it). A tags query with `@public` captures (patterns without a definition, so
//! other users of the query skip them) says which names are public: Rust's items with `pub`,
//! TypeScript's exported declarations.

use std::path::Path;

use vortex_core::syntax::{Loader, QueryMatchIterEvent};
use vortex_core::{Rope, Syntax};
use vortex_lsp::{lsp, util::pos_to_lsp_pos, OffsetEncoding};

use super::node::{Kind, Load, Node, Place, Provider};

struct Tag {
    name: String,
    kind: Kind,
    /// The definition's byte range.
    range: std::ops::Range<u32>,
    /// The name's start, in chars.
    name_start: usize,
    name_end: usize,
    name_byte: u32,
    public: Option<bool>,
}

/// The skeleton of `path` read from disk (or `text`, a buffer's), or `None` when its language
/// has no tags query.
pub(crate) fn skeleton(
    loader: &Loader,
    path: &Path,
    text: Option<Rope>,
    encoding: OffsetEncoding,
) -> Option<Vec<Node>> {
    let language = loader.language_for_filename(path)?;
    loader.tag_query(language)?;
    let text = match text {
        Some(text) => text,
        None => Rope::from(std::fs::read_to_string(path).ok()?),
    };
    let syntax = Syntax::new(text.slice(..), language, loader).ok()?;
    let mut tags = Vec::new();
    let mut public = std::collections::HashSet::new();
    let mut knows_public = false;
    let mut matches = syntax.tags(text.slice(..), loader, ..);
    while let Some(event) = matches.next() {
        let QueryMatchIterEvent::Match(found) = event else {
            continue;
        };
        let Some(query) = loader.tag_query(matches.current_language()) else {
            continue;
        };
        let query = &query.query;
        let name_capture = query.get_capture("name");
        knows_public |= query.get_capture("public").is_some();
        let mut definition = None;
        let mut name = None;
        for node in found.nodes.iter() {
            let capture = query.capture_name(node.capture);
            if let Some(kind) = capture.strip_prefix("definition.").and_then(Kind::from_tag) {
                definition = Some((kind, node.node.byte_range()));
            } else if Some(node.capture) == name_capture {
                name = Some(node.node.byte_range());
            } else if capture == "public" {
                public.insert(node.node.byte_range().start);
            }
        }
        let Some((kind, range)) = definition else {
            continue;
        };
        let name = name.unwrap_or_else(|| range.clone());
        let name_start = text.byte_to_char(name.start as usize);
        let name_end = text.byte_to_char(name.end as usize);
        tags.push(Tag {
            name: text.slice(name_start..name_end).to_string(),
            kind,
            range,
            name_start,
            name_end,
            name_byte: name.start,
            public: None,
        });
    }
    for tag in &mut tags {
        tag.public = knows_public.then(|| public.contains(&tag.name_byte));
    }
    tags.sort_by_key(|tag| {
        (
            tag.range.start,
            std::cmp::Reverse(tag.range.end),
            tag.name_start,
        )
    });
    tags.dedup_by(|later, earlier| later.name_start == earlier.name_start);
    let uri = lsp::Url::from_file_path(path).ok()?;
    let mut tags = tags.into_iter().peekable();
    Some(nest(&mut tags, None, &text, &uri, encoding))
}

/// The tags inside `parent`'s range (all of them at the top), in order.
fn nest(
    tags: &mut std::iter::Peekable<impl Iterator<Item = Tag>>,
    parent: Option<&std::ops::Range<u32>>,
    text: &Rope,
    uri: &lsp::Url,
    encoding: OffsetEncoding,
) -> Vec<Node> {
    let mut nodes = Vec::new();
    while let Some(tag) =
        tags.next_if(|tag| parent.is_none_or(|parent| tag.range.end <= parent.end))
    {
        let children = nest(tags, Some(&tag.range), text, uri, encoding);
        let mut node = Node::new(tag.name, tag.kind, Provider::Tags);
        node.public = tag.public;
        node.place = Some(Place {
            uri: uri.clone(),
            range: lsp::Range {
                start: pos_to_lsp_pos(text, tag.name_start, encoding),
                end: pos_to_lsp_pos(text, tag.name_end, encoding),
            },
            encoding,
        });
        if !children.is_empty() {
            node.load = Load::Given(children);
        }
        nodes.push(node);
    }
    nodes
}
