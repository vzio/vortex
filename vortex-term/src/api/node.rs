//! The nodes of the API browser's tree: what each item is, where it is, its path, and how its
//! children and docs are loaded.

use std::path::PathBuf;

use vortex_lsp::{lsp, OffsetEncoding};

/// What an item is, from the kinds of document symbols, workspace symbols and completion items
/// (which differ a little), of tree-sitter tags, and of the browser's own levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// One of the four roots.
    Root,
    Directory,
    File,
    Package,
    Module,
    Namespace,
    Type,
    Struct,
    Enum,
    /// A trait or an interface.
    Interface,
    Class,
    /// An `impl` block, an object literal: something that holds members.
    Object,
    Function,
    Method,
    Constructor,
    Field,
    Property,
    Variant,
    Constant,
    Variable,
    Macro,
    TypeParameter,
    Keyword,
    Snippet,
    Other,
}

impl Kind {
    pub fn from_symbol(kind: lsp::SymbolKind) -> Self {
        match kind {
            lsp::SymbolKind::FILE => Self::File,
            lsp::SymbolKind::MODULE => Self::Module,
            lsp::SymbolKind::NAMESPACE => Self::Namespace,
            lsp::SymbolKind::PACKAGE => Self::Package,
            lsp::SymbolKind::CLASS => Self::Class,
            lsp::SymbolKind::METHOD => Self::Method,
            lsp::SymbolKind::PROPERTY => Self::Property,
            lsp::SymbolKind::FIELD => Self::Field,
            lsp::SymbolKind::CONSTRUCTOR => Self::Constructor,
            lsp::SymbolKind::ENUM => Self::Enum,
            lsp::SymbolKind::INTERFACE => Self::Interface,
            lsp::SymbolKind::FUNCTION => Self::Function,
            lsp::SymbolKind::VARIABLE => Self::Variable,
            lsp::SymbolKind::CONSTANT => Self::Constant,
            lsp::SymbolKind::OBJECT => Self::Object,
            lsp::SymbolKind::ENUM_MEMBER => Self::Variant,
            lsp::SymbolKind::STRUCT => Self::Struct,
            lsp::SymbolKind::TYPE_PARAMETER => Self::TypeParameter,
            _ => Self::Other,
        }
    }

    pub fn from_completion(kind: Option<lsp::CompletionItemKind>) -> Self {
        let Some(kind) = kind else {
            return Self::Other;
        };
        match kind {
            lsp::CompletionItemKind::METHOD => Self::Method,
            lsp::CompletionItemKind::FUNCTION => Self::Function,
            lsp::CompletionItemKind::CONSTRUCTOR => Self::Constructor,
            lsp::CompletionItemKind::FIELD => Self::Field,
            lsp::CompletionItemKind::VARIABLE => Self::Variable,
            lsp::CompletionItemKind::CLASS => Self::Class,
            lsp::CompletionItemKind::INTERFACE => Self::Interface,
            lsp::CompletionItemKind::MODULE => Self::Module,
            lsp::CompletionItemKind::PROPERTY => Self::Property,
            lsp::CompletionItemKind::ENUM => Self::Enum,
            lsp::CompletionItemKind::KEYWORD => Self::Keyword,
            lsp::CompletionItemKind::SNIPPET => Self::Snippet,
            lsp::CompletionItemKind::FILE => Self::File,
            lsp::CompletionItemKind::FOLDER => Self::Directory,
            lsp::CompletionItemKind::ENUM_MEMBER => Self::Variant,
            lsp::CompletionItemKind::CONSTANT => Self::Constant,
            lsp::CompletionItemKind::STRUCT => Self::Struct,
            lsp::CompletionItemKind::TYPE_PARAMETER => Self::TypeParameter,
            lsp::CompletionItemKind::VALUE | lsp::CompletionItemKind::UNIT => Self::Constant,
            lsp::CompletionItemKind::REFERENCE => Self::Variable,
            _ => Self::Other,
        }
    }

    /// The kinds of `tags.scm` (`@definition.function` is `function`).
    pub fn from_tag(name: &str) -> Option<Self> {
        Some(match name {
            "class" => Self::Class,
            "constant" => Self::Constant,
            "enum" => Self::Enum,
            "field" => Self::Field,
            "function" => Self::Function,
            "interface" => Self::Interface,
            "macro" => Self::Macro,
            "module" => Self::Module,
            "struct" => Self::Struct,
            "type" => Self::Type,
            "method" => Self::Method,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Root => "",
            Self::Directory => "dir",
            Self::File => "file",
            Self::Package => "package",
            Self::Module => "module",
            Self::Namespace => "namespace",
            Self::Type => "type",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Interface => "interface",
            Self::Class => "class",
            Self::Object => "object",
            Self::Function => "function",
            Self::Method => "method",
            Self::Constructor => "constructor",
            Self::Field => "field",
            Self::Property => "property",
            Self::Variant => "variant",
            Self::Constant => "constant",
            Self::Variable => "variable",
            Self::Macro => "macro",
            Self::TypeParameter => "type param",
            Self::Keyword => "keyword",
            Self::Snippet => "snippet",
            Self::Other => "",
        }
    }

    /// A module, a namespace or a package: its members are listed like a package's.
    pub fn is_module(self) -> bool {
        matches!(self, Self::Module | Self::Namespace | Self::Package)
    }

    /// A type with members (fields, methods, variants), listed from the document symbols of the
    /// file that defines it.
    pub fn is_type(self) -> bool {
        matches!(
            self,
            Self::Struct | Self::Enum | Self::Interface | Self::Class | Self::Type
        )
    }
}

/// Where an item is: its file and the range of its name, in the server's offset encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    pub uri: lsp::Url,
    pub range: lsp::Range,
    pub encoding: OffsetEncoding,
}

impl Place {
    pub fn path(&self) -> Option<PathBuf> {
        self.uri.to_file_path().ok()
    }
}

/// Which request (or other source) gave an item, for the preview and the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    /// The browser's own levels: the roots, `api-roots`.
    Browser,
    /// A directory listing of the workspace.
    Files,
    /// `textDocument/documentSymbol`.
    DocumentSymbols,
    /// `tags.scm`, before the document symbols come.
    Tags,
    /// `workspace/symbol` with a path query (`std::collections::`).
    PathQuery,
    /// `textDocument/completion` at the cursor, in your buffer.
    Completion,
    /// `textDocument/completion` (or `definition`) in a temporary document.
    Probe,
    /// `textDocument/definition`, then the document symbols of the file it leads to.
    Definition,
}

impl Provider {
    pub fn label(self) -> &'static str {
        match self {
            Self::Browser => "browser",
            Self::Files => "files",
            Self::DocumentSymbols => "document symbols",
            Self::Tags => "tree-sitter tags",
            Self::PathQuery => "workspace symbols (path query)",
            Self::Completion => "completion at the cursor",
            Self::Probe => "completion in a temporary document",
            Self::Definition => "definition + document symbols",
        }
    }
}

impl Provider {
    /// A word or two, for the picker's prompt row.
    pub fn short(self) -> &'static str {
        match self {
            Self::Browser => "",
            Self::Files => "files",
            Self::DocumentSymbols => "symbols",
            Self::Tags => "tags",
            Self::PathQuery => "path query",
            Self::Completion => "completion",
            Self::Probe => "probe",
            Self::Definition => "definition",
        }
    }
}

/// The four roots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Root {
    Codebase,
    Scope,
    Packages,
    Std,
}

impl Root {
    pub const ALL: [Root; 4] = [Root::Codebase, Root::Scope, Root::Packages, Root::Std];

    pub fn name(self) -> &'static str {
        match self {
            Self::Codebase => "Codebase",
            Self::Scope => "Scope",
            Self::Packages => "Packages",
            Self::Std => "Std",
        }
    }
}

/// How a node's children are loaded.
#[derive(Debug, Clone)]
pub(crate) enum Load {
    /// A leaf.
    Nothing,
    Root(Root),
    /// Known with the node (nested document symbols): opening it asks nothing.
    Given(Vec<Node>),
    /// The entries of a workspace directory.
    Dir(PathBuf),
    /// The symbols of a file.
    File(PathBuf),
    /// A workspace symbol query that lists a module's items (`std::collections::`).
    PathQuery(String),
    /// The packages under a directory of packages (`net/`, empty at the top), from the probe's
    /// `packages` template.
    Packages(String),
    /// The members of a package (`std` uses the probe's `std` template), then of its members
    /// along `chain` (`["Router"]` for `express.Router.`).
    Members {
        module: String,
        chain: Vec<String>,
        std: bool,
    },
    /// The members of a type: the document symbols of the file that defines it.
    TypeMembers,
}

/// An item of the tree.
#[derive(Debug, Clone)]
pub struct Node {
    pub name: String,
    pub kind: Kind,
    /// The signature or detail the server gave.
    pub detail: Option<String>,
    /// Where it is defined, when known.
    pub place: Option<Place>,
    /// Its path from its root's top (`["std", "collections", "HashMap", "insert"]`).
    pub path: Vec<String>,
    /// From tree-sitter, where the tags query says it.
    pub public: Option<bool>,
    pub provider: Provider,
    pub(crate) load: Load,
    /// The completion item it came from, for its docs (`completionItem/resolve`).
    pub(crate) item: Option<Box<lsp::CompletionItem>>,
    /// Docs that came with it (a completion item's `documentation`).
    pub(crate) docs: Option<String>,
    /// For a member found by completion in a temporary document: the package and the chain of
    /// members it is in, to find its definition with another temporary document.
    pub(crate) probe: Option<(String, Vec<String>, bool)>,
}

impl Node {
    pub(crate) fn new(name: impl Into<String>, kind: Kind, provider: Provider) -> Self {
        Self {
            name: name.into(),
            kind,
            detail: None,
            place: None,
            path: Vec::new(),
            public: None,
            provider,
            load: Load::Nothing,
            item: None,
            docs: None,
            probe: None,
        }
    }

    pub fn has_children(&self) -> bool {
        match &self.load {
            Load::Nothing => false,
            Load::Given(children) => !children.is_empty(),
            _ => true,
        }
    }

    /// Whether opening the node asks a language server for something.
    pub fn asks_server(&self) -> bool {
        !matches!(
            self.load,
            Load::Nothing | Load::Given(_) | Load::Dir(_) | Load::Root(_)
        )
    }

    /// Its path, joined with `separator` (a file's path stays as it is).
    pub fn display_path(&self, separator: &str) -> String {
        self.path.join(separator)
    }
}

/// The text of a completion item's or a hover's documentation.
pub(crate) fn documentation_text(documentation: &lsp::Documentation) -> String {
    match documentation {
        lsp::Documentation::String(text) => text.clone(),
        lsp::Documentation::MarkupContent(content) => content.value.clone(),
    }
}

/// The name of a completion item, without the decorations some servers put around it (clangd's
/// leading space or `•`, the closing `>` or `"` of an include path).
pub(crate) fn completion_name(item: &lsp::CompletionItem) -> String {
    item.label
        .trim_start_matches(|c: char| c.is_whitespace() || c == '•')
        .trim_end_matches(['"', '\'', '>', '`'])
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_names_lose_their_decorations() {
        let item = |label: &str| lsp::CompletionItem {
            label: label.to_string(),
            ..Default::default()
        };
        assert_eq!(completion_name(&item(" Point")), "Point");
        assert_eq!(completion_name(&item("•printf")), "printf");
        assert_eq!(completion_name(&item("stdio.h>")), "stdio.h");
        assert_eq!(completion_name(&item("sys/")), "sys/");
        assert_eq!(completion_name(&item("express")), "express");
    }

    #[test]
    fn kinds_from_the_three_sources_agree() {
        assert_eq!(Kind::from_symbol(lsp::SymbolKind::STRUCT), Kind::Struct);
        assert_eq!(
            Kind::from_completion(Some(lsp::CompletionItemKind::STRUCT)),
            Kind::Struct
        );
        assert_eq!(Kind::from_tag("struct"), Some(Kind::Struct));
        assert!(Kind::Module.is_module() && !Kind::Module.is_type());
        assert!(Kind::Interface.is_type());
    }
}
