use crate::{utils, App};
use itertools::Itertools;
use lsp_types::{
    CompletionItem, CompletionList, CompletionResponse, CompletionTextEdit, Documentation, Range,
    TextDocumentPositionParams, TextEdit,
};
use manix::{DocEntry, DocSource};
use rnix::{
    types::{ParsedType, TokenWrapper, TypedNode},
    NixLanguage, SyntaxKind, SyntaxNode, TextUnit,
};
use std::convert::TryFrom;

impl App {
    fn scope_completions(
        &mut self,
        params: &TextDocumentPositionParams,
    ) -> Option<Vec<CompletionItem>> {
        let (ast, content) = self.files.get(&params.text_document.uri)?;
        let offset = utils::lookup_pos(content, params.position)?;
        let root_node = ast.node();

        let (name, scope) =
            self.scope_for_ident(params.text_document.uri.clone(), &root_node, offset)?;
        let (_, content) = self.files.get(&params.text_document.uri)?;

        let scope_completions = scope
            .keys()
            .filter(|var| var.starts_with(&name.as_str()))
            .map(|var| CompletionItem {
                label: var.clone(),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range: utils::range(content, name.node().text_range()),
                    new_text: var.clone(),
                })),
                ..CompletionItem::default()
            })
            .collect_vec();
        Some(scope_completions)
    }

    fn manix_options_completions(
        &self,
        params: &TextDocumentPositionParams,
    ) -> Option<Vec<CompletionItem>> {
        // TODO implement this
        None
    }

    fn manix_value_completions(
        &self,
        params: &TextDocumentPositionParams,
    ) -> Option<Vec<CompletionItem>> {
        let (ast, content) = self.files.get(&params.text_document.uri)?;
        let offset = utils::lookup_pos(content, params.position)?;
        let root_node = ast.node();

        let node = utils::closest_node_to(&root_node, offset)?;
        dbg!(&node);
        let (full_ident_node, full_ident_name) = self
            .full_ident_name(&node)
            .unwrap_or((node.clone(), vec![node.text().to_string()]));
        dbg!(&full_ident_node, &full_ident_name);

        let node_range = Range {
            start: utils::offset_to_pos(
                content,
                full_ident_node
                    .first_token()?
                    .text_range()
                    .start()
                    .to_usize(),
            ),
            end: utils::offset_to_pos(
                content,
                full_ident_node
                    .descendants_with_tokens()
                    .take_while(|n| match n {
                        rnix::NodeOrToken::Node(_) => true,
                        rnix::NodeOrToken::Token(t) => {
                            t.kind() == SyntaxKind::TOKEN_DOT || t.kind() == SyntaxKind::TOKEN_IDENT
                        }
                    })
                    .last()?
                    .text_range()
                    .end()
                    .to_usize(),
            ),
        };
        let node_range: Range = Range {
            start: utils::offset_to_pos(content, node.text_range().start().to_usize()),
            end: utils::offset_to_pos(content, node.text_range().end().to_usize()),
        };

        // let search_results = std::collections::HashSet::<DocEntry>::new();
        // for res in self.manix_values.search(&full_ident_name.join(".")) {
        //     search_results.insert(res);
        // }
        let mut strip_val = std::collections::HashMap::<String, String>::new();
        let mut search_results: Vec<(String, DocEntry)> = self
            .manix_values
            .search(&manix::Lowercase(
                full_ident_name.join(".").to_ascii_lowercase().as_bytes(),
            ))
            .into_iter()
            .map(|e| (full_ident_name.join(".").to_owned(), e))
            .collect();
        for r in search_results.iter() {
            let mut namespace = full_ident_name.clone();
            namespace.pop();
            strip_val.insert(r.1.name(), namespace.join(".") + ".");
        }
        if let Some((_, possible_namespaces)) =
            utils::scope_for(&std::rc::Rc::new(params.text_document.uri.clone()), node)
        {
            dbg!(&possible_namespaces);
            for possible_namespace in possible_namespaces {
                let mut full_path = String::new();
                full_path.push_str(&possible_namespace);
                full_path.push('.');
                full_path.push_str(&full_ident_name.join("."));
                dbg!(&full_path);

                let mut results: Vec<(String, DocEntry)> = self
                    .manix_values
                    .search(&manix::Lowercase(full_path.to_ascii_lowercase().as_bytes()))
                    .into_iter()
                    .map(|e| (full_path.clone(), e))
                    .collect();
                dbg!(&results);

                let mut x: Vec<&str> = full_path.split(".").collect();
                x.pop();
                for (_, doc) in results.iter() {
                    let namespace = x.clone();
                    if let Some(old) = strip_val.get(&doc.name()) {
                        if old.len() > x.len() {
                            continue;
                        }
                    }
                    strip_val.insert(doc.name(), x.join(".") + ".");
                }

                search_results.append(&mut results);
            }
        }
        let search_results = search_results
            .into_iter()
            // .sorted_by(|a, b| Ord::cmp(&a.0.len(), &b.0.len()).reverse())
            .unique_by(|(_, entry)| entry.name())
            .into_group_map()
            .into_iter()
            .collect_vec();

        let mut manix_completions = Vec::<CompletionItem>::new();

        for (path, search_results) in search_results {
            let (namespace, namespace_items) = self.next_namespace_step_completions(
                path.split(".").map(|s| s.to_owned()).collect(),
                search_results,
            );

            let mut completions = namespace_items
                .iter()
                .unique_by(|x| x.name())
                .map(|def| CompletionItem {
                    label: if let Some(namespace) = strip_val.get(&def.name()) {
                        eprintln!("stripping {} in {}", &namespace, def.name());
                        def.name()
                            .strip_prefix(namespace)
                            .unwrap_or(&path)
                            .to_owned()
                    } else {
                        def.name()
                    },
                    text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                        range: node_range,
                        new_text: def.name().clone(),
                    })),
                    documentation: def
                        .try_as_doc_entry()
                        .map(|entry| Documentation::String(entry.pretty_printed())),
                    ..CompletionItem::default()
                })
                .collect_vec();
            manix_completions.append(&mut completions);
        }
        Some(manix_completions)
    }

    #[allow(clippy::shadow_unrelated)] // false positive
    pub fn completions(
        &mut self,
        params: &TextDocumentPositionParams,
    ) -> Option<Vec<CompletionItem>> {
        // let scope_completions = self.scope_completions(params)?;
        let mut manix_value_completions = self.manix_value_completions(params).unwrap_or_default();
        let mut manix_options_completions =
            self.manix_options_completions(params).unwrap_or_default();
        let mut completions = Vec::new();
        completions.append(&mut manix_value_completions);
        completions.append(&mut manix_options_completions);

        Some(completions)
    }

    fn next_namespace_step_completions(
        &self,
        current_ns: Vec<String>,
        search_results: Vec<DocEntry>,
    ) -> (Vec<String>, Vec<NamespaceCompletionResult>) {
        // TODO handle things like `with pkgs;`

        let query_ns_iter = current_ns.iter();
        let longest_match = search_results
            .iter()
            .map(|result| {
                result
                    .name()
                    .split('.')
                    .zip(query_ns_iter.clone())
                    // .take_while(|(a, b)| a == b)
                    .map(|(a, _)| a.to_string())
                    .collect_vec()
            })
            .max();
        if let Some(longest_match) = longest_match {
            dbg!(&current_ns, &longest_match);
            let completions = search_results
                .into_iter()
                // .filter(|result| {
                //     result
                //         .name()
                //         .split('.')
                //         .zip(query_ns_iter.clone())
                //         .take_while(|(a, b)| a == b)
                //         .count()
                //         > 0
                // })
                .map(|result| {
                    use NamespaceCompletionResult::*;
                    if result.name().split('.').count() - 1 == longest_match.len() {
                        FinalNode(result)
                    } else {
                        let presented_result =
                            result.name().split('.').take(longest_match.len()).join(".");
                        Set(presented_result)
                    }
                })
                .unique_by(|x| x.name())
                .collect_vec();
            (current_ns, completions)
        } else {
            (current_ns, Vec::new())
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum NamespaceCompletionResult {
    Set(String),
    FinalNode(DocEntry),
}

impl NamespaceCompletionResult {
    fn name(&self) -> String {
        use NamespaceCompletionResult::*;
        match self {
            Set(s) => s.to_owned(),
            FinalNode(entry) => entry.name(),
        }
    }

    fn try_as_doc_entry(&self) -> Option<&DocEntry> {
        use NamespaceCompletionResult::*;
        match self {
            Set(_) => None,
            FinalNode(entry) => Some(entry),
        }
    }
}
