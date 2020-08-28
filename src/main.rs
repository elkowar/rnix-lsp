#![warn(
    // Harden built-in lints
    missing_copy_implementations,
    missing_debug_implementations,

    // Harden clippy lints
    clippy::cargo_common_metadata,
    clippy::clone_on_ref_ptr,
    clippy::dbg_macro,
    clippy::decimal_literal_representation,
    clippy::float_cmp_const,
    clippy::get_unwrap,
    clippy::integer_arithmetic,
    clippy::integer_division,
    clippy::pedantic,
)]
#![allow(
    // filter().map() can sometimes be more readable
    clippy::filter_map,
    // Most integer arithmetics are within an allocated region, so we know it's safe
    clippy::integer_arithmetic,
)]

mod completion;
mod lookup;
mod utils;

use dirs::home_dir;
use itertools::Itertools;
use log::{error, trace, warn};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
use lsp_types::{
    notification::{Notification as _, *},
    request::{Request as RequestTrait, *},
    *,
};
use manix::{
    comments_docsource::CommentsDatabase,
    nixpkgs_tree_docsource,
    options_docsource::{self, OptionsDatabase},
    xml_docsource, AggregateDocSource, Cache, DocSource,
};
use nixpkgs_tree_docsource::NixpkgsTreeDatabase;
use rnix::{
    parser::*,
    types::*,
    value::{Anchor as RAnchor, Value as RValue},
    SyntaxNode,
};
use std::{
    collections::HashMap,
    fs, panic,
    path::{Path, PathBuf},
    process,
    rc::Rc,
};
use xml_docsource::XmlFuncDocDatabase;

type Error = Box<dyn std::error::Error>;

fn main() {
    if let Err(err) = real_main() {
        error!("Error: {} ({:?})", err, err);
        error!("A fatal error has occured and rnix-lsp will shut down.");
        drop(err);
        process::exit(libc::EXIT_FAILURE);
    }
}
fn real_main() -> Result<(), Error> {
    env_logger::init();
    panic::set_hook(Box::new(move |panic| {
        error!("----- Panic -----");
        error!("{}", panic);
    }));

    let (connection, io_threads) = Connection::stdio();
    let capabilities = serde_json::to_value(&ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::Full),
                ..TextDocumentSyncOptions::default()
            },
        )),
        completion_provider: Some(CompletionOptions {
            ..CompletionOptions::default()
        }),
        definition_provider: Some(true),
        document_formatting_provider: Some(true),
        document_link_provider: Some(DocumentLinkOptions {
            resolve_provider: Some(false),
            work_done_progress_options: WorkDoneProgressOptions::default(),
        }),
        rename_provider: Some(RenameProviderCapability::Simple(true)),
        selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        ..ServerCapabilities::default()
    })
    .unwrap();

    connection.initialize(capabilities)?;

    let (cache_invalid, manix_values) = load_manix_values().unwrap();
    let manix_options = load_manix_options(cache_invalid).unwrap();
    App {
        files: HashMap::new(),
        manix_options,
        manix_values,
        conn: connection,
    }
    .main();

    io_threads.join()?;

    Ok(())
}

fn build_source_and_add<T>(
    mut source: T,
    name: &str,
    path: &PathBuf,
    aggregate: &mut AggregateDocSource,
) where
    T: 'static + DocSource + manix::Cache + Sync,
{
    eprintln!("Building {} cache...", name);
    if let Err(e) = source.update() {
        eprintln!("{:?}", e);
        return;
    }

    if let Err(e) = source.save(&path) {
        eprintln!("{:?}", e);
        return;
    }

    aggregate.add_source(Box::new(source));
}

fn load_manix_values() -> Option<(bool, AggregateDocSource)> {
    let cache_dir = xdg::BaseDirectories::with_prefix("manix").ok()?;
    let comment_cache_path = cache_dir.place_cache_file("database.bin").ok()?;
    let nixpkgs_tree_cache_path = cache_dir.place_cache_file("nixpkgs_tree.bin").ok()?;
    let nixpkgs_doc_cache_path = cache_dir
        .place_cache_file("nixpkgs_doc_database.bin")
        .ok()?;
    let mut aggregate_source = AggregateDocSource::default();

    let mut comment_db = if comment_cache_path.exists() {
        CommentsDatabase::load(&std::fs::read(&comment_cache_path).ok()?).ok()?
    } else {
        CommentsDatabase::new()
    };
    if comment_db.hash_to_defs.len() == 0 {
        eprintln!("Building NixOS comments cache...");
    }
    let cache_invalid = comment_db.update().ok()?;
    comment_db.save(&comment_cache_path).ok()?;
    aggregate_source.add_source(Box::new(comment_db));
    if cache_invalid {
        build_source_and_add(
            nixpkgs_tree_docsource::NixpkgsTreeDatabase::new(),
            "Nixpkgs Tree",
            &nixpkgs_tree_cache_path,
            &mut aggregate_source,
        );

        build_source_and_add(
            xml_docsource::XmlFuncDocDatabase::new(),
            "Nixpkgs Documentation",
            &nixpkgs_doc_cache_path,
            &mut aggregate_source,
        );
    } else {
        aggregate_source.add_source(Box::new(
            NixpkgsTreeDatabase::load(&fs::read(&nixpkgs_tree_cache_path).ok()?).ok()?,
        ));

        aggregate_source.add_source(Box::new(
            XmlFuncDocDatabase::load(&fs::read(&nixpkgs_doc_cache_path).ok()?).ok()?,
        ));
    }
    Some((cache_invalid, aggregate_source))
}

fn load_manix_options(reload_cache: bool) -> Option<AggregateDocSource> {
    let cache_dir = xdg::BaseDirectories::with_prefix("manix").ok()?;

    let options_hm_cache_path = cache_dir.place_cache_file("options_hm_database.bin").ok()?;
    let options_nixos_cache_path = cache_dir
        .place_cache_file("options_nixos_database.bin")
        .ok()?;

    let mut aggregate_source = AggregateDocSource::default();

    if reload_cache {
        build_source_and_add(
            OptionsDatabase::new(options_docsource::OptionsDatabaseType::HomeManager),
            "Home Manager Options",
            &options_hm_cache_path,
            &mut aggregate_source,
        );

        build_source_and_add(
            OptionsDatabase::new(options_docsource::OptionsDatabaseType::NixOS),
            "NixOS Options",
            &options_nixos_cache_path,
            &mut aggregate_source,
        );
    } else {
        aggregate_source.add_source(Box::new(
            OptionsDatabase::load(&fs::read(&options_hm_cache_path).ok()?).ok()?,
        ));

        aggregate_source.add_source(Box::new(
            OptionsDatabase::load(&fs::read(&options_nixos_cache_path).ok()?).ok()?,
        ));
    }
    Some(aggregate_source)
}

struct App {
    files: HashMap<Url, (AST, String)>,
    manix_options: manix::AggregateDocSource,
    manix_values: manix::AggregateDocSource,
    conn: Connection,
}
impl App {
    fn reply(&mut self, response: Response) {
        trace!("Sending response: {:#?}", response);
        self.conn.sender.send(Message::Response(response)).unwrap();
    }
    fn notify(&mut self, notification: Notification) {
        trace!("Sending notification: {:#?}", notification);
        self.conn
            .sender
            .send(Message::Notification(notification))
            .unwrap();
    }
    fn err<E>(&mut self, id: RequestId, err: E)
    where
        E: std::fmt::Display,
    {
        warn!("{}", err);
        self.reply(Response::new_err(
            id,
            ErrorCode::UnknownErrorCode as i32,
            err.to_string(),
        ));
    }
    fn main(&mut self) {
        while let Ok(msg) = self.conn.receiver.recv() {
            trace!("Message: {:#?}", msg);
            match msg {
                Message::Request(req) => {
                    let id = req.id.clone();
                    match self.conn.handle_shutdown(&req) {
                        Ok(true) => break,
                        Ok(false) => {
                            if let Err(err) = self.handle_request(req) {
                                self.err(id, err);
                            }
                        }
                        Err(err) => {
                            // This only fails if a shutdown was
                            // requested in the first place, so it
                            // should definitely break out of the
                            // loop.
                            self.err(id, err);
                            break;
                        }
                    }
                }
                Message::Notification(notification) => {
                    let _ = self.handle_notification(notification);
                }
                Message::Response(_) => (),
            }
        }
    }
    fn handle_request(&mut self, req: Request) -> Result<(), Error> {
        fn cast<Kind>(req: &mut Option<Request>) -> Option<(RequestId, Kind::Params)>
        where
            Kind: RequestTrait,
            Kind::Params: serde::de::DeserializeOwned,
        {
            match req.take().unwrap().extract::<Kind::Params>(Kind::METHOD) {
                Ok(value) => Some(value),
                Err(owned) => {
                    *req = Some(owned);
                    None
                }
            }
        }
        let mut req = Some(req);
        if let Some((id, params)) = cast::<GotoDefinition>(&mut req) {
            if let Some(pos) = self.lookup_definition(params.text_document_position_params) {
                self.reply(Response::new_ok(id, pos));
            } else {
                self.reply(Response::new_ok(id, ()));
            }
        } else if let Some((id, params)) = cast::<HoverRequest>(&mut req) {
            let documentation = self
                .documentation(&params.text_document_position_params)
                .unwrap_or_default();
            self.reply(Response::new_ok(
                id,
                Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: documentation,
                    }),
                    range: None,
                },
            ));
        } else if let Some((id, params)) = cast::<Completion>(&mut req) {
            // look at params.context for trigger reasons, etc
            let completions = self
                .completions(&params.text_document_position)
                .unwrap_or_default();
            // .unwrap_or_else(|| CompletionResponse::Array(Vec::new()));
            self.reply(Response::new_ok(id, completions));
        } else if let Some((id, params)) = cast::<Rename>(&mut req) {
            let changes = self.rename(params);
            self.reply(Response::new_ok(
                id,
                WorkspaceEdit {
                    changes,
                    ..WorkspaceEdit::default()
                },
            ));
        } else if let Some((id, params)) = cast::<DocumentLinkRequest>(&mut req) {
            let document_links = self.document_links(&params).unwrap_or_default();
            self.reply(Response::new_ok(id, document_links));
        } else if let Some((id, params)) = cast::<Formatting>(&mut req) {
            let changes = if let Some((ast, code)) = self.files.get(&params.text_document.uri) {
                let fmt = nixpkgs_fmt::reformat_node(&ast.node());
                fmt.text_diff()
                    .iter()
                    .filter(|range| !range.delete.is_empty() || !range.insert.is_empty())
                    .map(|edit| TextEdit {
                        range: utils::range(&code, edit.delete),
                        new_text: edit.insert.to_string(),
                    })
                    .collect()
            } else {
                Vec::new()
            };
            self.reply(Response::new_ok(id, changes));
        } else if let Some((id, params)) = cast::<SelectionRangeRequest>(&mut req) {
            let mut selections = Vec::new();
            if let Some((ast, code)) = self.files.get(&params.text_document.uri) {
                for pos in params.positions {
                    selections.push(utils::selection_ranges(&ast.node(), code, pos));
                }
            }
            self.reply(Response::new_ok(id, selections));
        }
        Ok(())
    }
    fn handle_notification(&mut self, req: Notification) -> Result<(), Error> {
        match &*req.method {
            DidOpenTextDocument::METHOD => {
                let params: DidOpenTextDocumentParams = serde_json::from_value(req.params)?;
                let text = params.text_document.text;
                let parsed = rnix::parse(&text);
                self.send_diagnostics(params.text_document.uri.clone(), &text, &parsed)?;
                self.files.insert(params.text_document.uri, (parsed, text));
            }
            DidChangeTextDocument::METHOD => {
                let params: DidChangeTextDocumentParams = serde_json::from_value(req.params)?;
                if let Some(change) = params.content_changes.into_iter().last() {
                    let parsed = rnix::parse(&change.text);
                    self.send_diagnostics(params.text_document.uri.clone(), &change.text, &parsed)?;
                    self.files
                        .insert(params.text_document.uri, (parsed, change.text));
                }
            }
            _ => (),
        }
        Ok(())
    }
    fn lookup_definition(&mut self, params: TextDocumentPositionParams) -> Option<Location> {
        let (current_ast, current_content) = self.files.get(&params.text_document.uri)?;
        let offset = utils::lookup_pos(current_content, params.position)?;
        let node = current_ast.node();
        let (name, scope) = self.scope_for_ident(params.text_document.uri, &node, offset)?;

        let var = scope.get(name.as_str())?;
        let (_definition_ast, definition_content) = self.files.get(&var.file)?;
        Some(Location {
            uri: (*var.file).clone(),
            range: utils::range(definition_content, var.key.text_range()),
        })
    }

    fn documentation(&mut self, params: &TextDocumentPositionParams) -> Option<String> {
        let (ast, content) = self.files.get(&params.text_document.uri)?;
        let offset = utils::lookup_pos(content, params.position)?;
        let cursor = utils::ident_at(&ast.node(), offset)?;
        let ident = cursor.ident.as_str();

        let ident_lower = ident.to_ascii_lowercase();
        let query = manix::Lowercase(ident_lower.as_bytes());
        let mut definitions = self.manix_values.search(&query);
        definitions.append(&mut self.manix_options.search(&query));

        Some(
            definitions
                .iter()
                .map(|def| def.pretty_printed())
                .collect::<Vec<String>>()
                .join("\n"),
        )
    }

    fn rename(&mut self, params: RenameParams) -> Option<HashMap<Url, Vec<TextEdit>>> {
        struct Rename<'a> {
            edits: Vec<TextEdit>,
            code: &'a str,
            old: &'a str,
            new_name: String,
        }
        fn rename_in_node(rename: &mut Rename, node: &SyntaxNode) -> Option<()> {
            if let Some(ident) = Ident::cast(node.clone()) {
                if ident.as_str() == rename.old {
                    rename.edits.push(TextEdit {
                        range: utils::range(rename.code, node.text_range()),
                        new_text: rename.new_name.clone(),
                    });
                }
            } else if let Some(index) = Select::cast(node.clone()) {
                rename_in_node(rename, &index.set()?);
            } else if let Some(attr) = Key::cast(node.clone()) {
                let mut path = attr.path();
                if let Some(ident) = path.next() {
                    rename_in_node(rename, &ident);
                }
            } else {
                for child in node.children() {
                    rename_in_node(rename, &child);
                }
            }
            Some(())
        }

        let uri = params.text_document_position.text_document.uri;
        let (ast, code) = self.files.get(&uri)?;
        let offset = utils::lookup_pos(code, params.text_document_position.position)?;
        let info = utils::ident_at(&ast.node(), offset)?;
        if !info.path.is_empty() {
            // Renaming within a set not supported
            return None;
        }
        let old = info.ident;
        let (scope, _) = utils::scope_for(&Rc::new(uri.clone()), old.node().clone())?;

        let mut rename = Rename {
            edits: Vec::new(),
            code,
            old: old.as_str(),
            new_name: params.new_name,
        };
        let definition = scope.get(old.as_str())?;
        rename_in_node(&mut rename, &definition.set);

        let mut changes = HashMap::new();
        changes.insert(uri, rename.edits);
        Some(changes)
    }
    fn document_links(&mut self, params: &DocumentLinkParams) -> Option<Vec<DocumentLink>> {
        let (current_ast, current_content) = self.files.get(&params.text_document.uri)?;
        let parent_dir = Path::new(params.text_document.uri.path()).parent();
        let home_dir = home_dir();
        let home_dir = home_dir.as_ref();
        let mut document_links = vec![];
        for node in current_ast.node().descendants() {
            let value = Value::cast(node.clone()).and_then(|v| v.to_value().ok());
            if let Some(RValue::Path(anchor, path)) = value {
                let file_url = match anchor {
                    RAnchor::Absolute => Some(PathBuf::from(&path)),
                    RAnchor::Relative => parent_dir.map(|p| p.join(path)),
                    RAnchor::Home => home_dir.map(|home| home.join(path)),
                    RAnchor::Store => None,
                }
                .and_then(|path| std::fs::canonicalize(&path).ok())
                .filter(|path| path.is_file())
                .and_then(|s| Url::parse(&format!("file://{}", s.to_string_lossy())).ok());
                if let Some(file_url) = file_url {
                    document_links.push(DocumentLink {
                        target: Some(file_url),
                        range: utils::range(current_content, node.text_range()),
                        tooltip: None,
                        data: None,
                    })
                }
            }
        }
        Some(document_links)
    }
    fn send_diagnostics(&mut self, uri: Url, code: &str, ast: &AST) -> Result<(), Error> {
        let errors = ast.errors();
        let mut diagnostics = Vec::with_capacity(errors.len());
        for err in errors {
            if let ParseError::Unexpected(node) = err {
                diagnostics.push(Diagnostic {
                    range: utils::range(code, node),
                    severity: Some(DiagnosticSeverity::Error),
                    message: err.to_string(),
                    ..Diagnostic::default()
                });
            }
        }
        self.notify(Notification::new(
            "textDocument/publishDiagnostics".into(),
            PublishDiagnosticsParams {
                uri,
                diagnostics,
                version: None,
            },
        ));
        Ok(())
    }
}
