use crate::{
    utils::{self, Var},
    App,
};
use lsp_types::Url;
use rnix::{types::*, value::Value as ParsedValue, NodeOrToken, SyntaxKind, SyntaxNode};
use std::{
    collections::{hash_map::Entry, HashMap},
    convert::TryFrom,
    fs,
    rc::Rc,
};

impl App {
    pub fn scope_for_ident(
        &mut self,
        file: Url,
        root: &SyntaxNode,
        offset: usize,
    ) -> Option<(Ident, HashMap<String, Var>)> {
        let mut file = Rc::new(file);
        let info = utils::ident_at(&root, offset)?;
        let ident = info.ident;
        let (mut entries, _) = utils::scope_for(&file, ident.node().clone())?;
        for var in info.path {
            let node = entries.get(&var)?.value.clone()?;
            entries = self.scope_from_node(&mut file, node)?;
        }
        Some((Ident::cast(ident.node().clone()).unwrap(), entries))
    }
    pub fn scope_from_node(
        &mut self,
        file: &mut Rc<Url>,
        mut node: SyntaxNode,
    ) -> Option<HashMap<String, Var>> {
        let mut scope = HashMap::new();

        if let Some(entry) = KeyValue::cast(node.clone()) {
            node = entry.value()?;
        }

        // Resolve simple imports
        loop {
            let apply = match Apply::cast(node.clone()) {
                None => break,
                Some(apply) => apply,
            };
            if Ident::cast(apply.lambda()?).map_or(true, |ident| ident.as_str() != "import") {
                break;
            }
            let (_anchor, path) = match Value::cast(apply.value()?) {
                None => break,
                Some(value) => match value.to_value() {
                    Ok(ParsedValue::Path(anchor, path)) => (anchor, path),
                    _ => break,
                },
            };

            // TODO use anchor
            *file = Rc::new(file.join(&path).ok()?);
            let path = utils::uri_path(&file)?;
            node = match self.files.entry((**file).clone()) {
                Entry::Occupied(entry) => {
                    let (ast, _code) = entry.get();
                    ast.root().inner()?.clone()
                }
                Entry::Vacant(placeholder) => {
                    let content = fs::read_to_string(&path).ok()?;
                    let ast = rnix::parse(&content);
                    let node = ast.root().inner()?.clone();
                    placeholder.insert((ast, content));
                    node
                }
            };
        }

        if let Some(set) = AttrSet::cast(node) {
            utils::populate(&file, &mut scope, &set);
        }
        Some(scope)
    }

    pub fn full_ident_name(&self, node: &SyntaxNode) -> Option<(SyntaxNode, Vec<String>)> {
        let try_get_ident_name = |x: SyntaxNode| match ParsedType::try_from(x) {
            Ok(ParsedType::Ident(ident)) => Some(ident.as_str().to_string()),
            _ => None,
        };

        let node_path_pair: Option<(SyntaxNode, Vec<String>)> = node.ancestors().find_map(|node| {
            let path = match ParsedType::try_from(node.clone()) {
                Ok(ParsedType::Key(key)) => {
                    let path = key
                        .node()
                        .children_with_tokens()
                        .take_while(|n| match n {
                            NodeOrToken::Node(n) => n.kind() == SyntaxKind::NODE_IDENT,
                            NodeOrToken::Token(t) => t.kind() == SyntaxKind::TOKEN_DOT,
                        })
                        .filter_map(|n| match n {
                            NodeOrToken::Token(t) if t.kind() == SyntaxKind::TOKEN_DOT => {
                                Some("".to_owned())
                            }
                            x => x.as_node().cloned().and_then(|n| try_get_ident_name(n)),
                        })
                        // .filter(|name| !name.trim().trim_end_matches("\n").is_empty())
                        .map(|x| x.replace("\n", ""))
                        .collect::<Vec<_>>();
                    Some(path)
                }
                _ => None,
            };
            path.map(|x| (node, x))
        });

        let node_path_pair = node_path_pair.or_else(|| {
            let mut outermost_select = None;
            for ancestor in node.ancestors() {
                match ParsedType::try_from(ancestor.clone()) {
                    Ok(ParsedType::Select(select)) => {
                        outermost_select = Some(select);
                    }
                    _ if outermost_select.is_some() => {
                        break;
                    }
                    _ => {}
                }
            }

            let mut path = Vec::new();
            for child in outermost_select.clone()?.node().descendants_with_tokens() {
                match child {
                    NodeOrToken::Node(_) => {}
                    NodeOrToken::Token(t) if t.kind() == SyntaxKind::TOKEN_DOT => {
                        path.push("".to_owned());
                    }
                    NodeOrToken::Token(t) if t.kind() == SyntaxKind::TOKEN_IDENT => {
                        path.push(t.text().to_string());
                    }
                    NodeOrToken::Token(_) => {
                        break;
                    }
                }
            }
            let last = path.len() - 1;
            let path = path
                .into_iter()
                .enumerate()
                .filter(|(i, p)| *i == last || p != "")
                .map(|(_, p)| p)
                .collect::<Vec<String>>();
            Some((outermost_select?.node().clone(), path))
        });

        // Ok(ParsedType::Select(key)) => {
        //     let path = key
        //         .node()
        //         .children_with_tokens()
        //         .take_while(|n| match n {
        //             NodeOrToken::Node(n) => n.kind() == SyntaxKind::NODE_IDENT,
        //             NodeOrToken::Token(t) => t.kind() == SyntaxKind::TOKEN_DOT,
        //         })
        //         .filter_map(|n| n.as_node().cloned())
        //         .filter_map(try_get_ident_name)
        //         .filter(|name| !name.trim().trim_end_matches("\n").is_empty())
        //         .map(|x| x.replace("\n", ""))
        //         .collect::<Vec<_>>();
        //     Some(path)
        // }
        dbg!(&node_path_pair);

        Some(node_path_pair?)
    }

    pub fn namespace_for_node(&self, node: &SyntaxNode) -> Vec<String> {
        let mut path = node
            .parent()
            .map(|p| self.namespace_for_node(&p))
            .unwrap_or_default();

        if let Ok(ParsedType::KeyValue(key_value)) = ParsedType::try_from(node.clone()) {
            let mut my_path = key_value
                .key()
                .unwrap()
                .path()
                .map(|x| x.to_string())
                .collect::<Vec<_>>();
            path.append(&mut my_path);
        }
        path
    }
}
