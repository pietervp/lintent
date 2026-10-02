//! Language-agnostic scope extraction.
//!
//! A file is cut into *units* — the classes, methods and functions a rule can
//! be asked about. Nothing here knows any particular language:
//!
//! 1. **Tags query** (preferred). Most grammars ship `queries/tags.scm` for
//!    code navigation. Its `@definition.*` captures are exactly the scopes we
//!    want: `class`/`interface`/`struct`/`enum`/`trait`/`object`/`type` and
//!    `module` become class units, `method` a method, `function` a function —
//!    or a method when its nearest enclosing definition is a class.
//!    lintent's own query additions may also capture `@name.member`, which
//!    is appended to the name (`loadFn.handler`).
//! 2. **Heuristic** (fallback for grammars without a tags query). Named nodes
//!    are classified by their node-type name: `*_{class,struct,interface,
//!    trait,impl,enum,object,module}_{declaration,definition,item,specifier}`
//!    are classes, `*{function,method,func,fun,procedure,sub}[_]{declaration,
//!    definition,item}` are functions (methods when inside a class). Grammar
//!    authors overwhelmingly follow these naming conventions, which is what
//!    makes an unknown grammar useful out of the box.
//!
//! Both paths feed the same post-processing, so units look identical however
//! they were found: the span is lifted over single-child wrappers (`export`,
//! decorated definitions, `const x = () => …`) and leading decorators /
//! attributes / annotations, so it starts where a reader sees it start.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tree_sitter::{Node, Parser, Query, QueryCursor, StreamingIterator};

use crate::languages::LanguageSpec;

/// Scopes longer than this are still judged, but only their head is sent:
/// the head carries the signature and the shape of the body, which is what
/// most rules look at, and Jev's context window is finite.
pub const MAX_SOURCE_LINES: usize = 400;
/// The same cut by size, for files with very long lines (≈15k tokens).
pub const MAX_SOURCE_CHARS: usize = 60_000;
/// The enclosing class is context, not the subject, so it is capped hard.
pub const MAX_PARENT_CHARS: usize = 4000;

/// The kind of scope a rule can target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScopeKind {
    Class,
    Method,
    Function,
}

impl ScopeKind {
    pub const ALL: [ScopeKind; 3] = [ScopeKind::Class, ScopeKind::Method, ScopeKind::Function];

    pub fn as_str(self) -> &'static str {
        match self {
            ScopeKind::Class => "class",
            ScopeKind::Method => "method",
            ScopeKind::Function => "function",
        }
    }
}

impl fmt::Display for ScopeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

impl FromStr for ScopeKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        ScopeKind::ALL
            .into_iter()
            .find(|kind| kind.as_str() == value.trim())
            .ok_or_else(|| format!("unknown scope {value:?} (expected class, method or function)"))
    }
}

/// One judgeable scope.
#[derive(Debug, Clone, PartialEq)]
pub struct Unit {
    pub kind: ScopeKind,
    /// Members are qualified with their container: `Class.method`.
    pub name: String,
    pub language: String,
    /// Repo-relative, `/`-separated.
    pub path: String,
    /// 1-based, inclusive.
    pub start_line: usize,
    pub end_line: usize,
    pub source: String,
    /// For methods: the enclosing class, truncated to [`MAX_PARENT_CHARS`].
    pub parent_source: Option<String>,
    /// True when `source` was cut to [`MAX_SOURCE_LINES`] / [`MAX_SOURCE_CHARS`].
    pub truncated: bool,
}

/// One line of a comment node, for keep marks.
#[derive(Debug, Clone, PartialEq)]
pub struct CommentLine {
    /// 1-based.
    pub line: usize,
    pub text: String,
}

/// Everything lintent needs from one parsed file.
#[derive(Debug, Clone, Default)]
pub struct ParsedFile {
    pub units: Vec<Unit>,
    /// Lines inside comment nodes (any node type containing "comment"), so
    /// keep marks work in every language without knowing its comment syntax.
    pub comments: Vec<CommentLine>,
    /// Blank and comment-only lines (1-based), for binding keep marks.
    pub filler_lines: std::collections::BTreeSet<usize>,
}

/// Parses `source` with `spec`'s grammar and extracts its units.
pub fn parse_file(path: &str, spec: &LanguageSpec, source: &str) -> Result<ParsedFile> {
    let loaded = spec.load()?;
    parse_with(
        path,
        &spec.name,
        &loaded.grammar,
        loaded.tags.as_ref(),
        source,
    )
}

/// The engine behind [`parse_file`]; `tags = None` selects the heuristic.
pub fn parse_with(
    path: &str,
    language: &str,
    grammar: &tree_sitter::Language,
    tags: Option<&Query>,
    source: &str,
) -> Result<ParsedFile> {
    let mut parser = Parser::new();
    parser
        .set_language(grammar)
        .with_context(|| format!("the {language} grammar is incompatible with this tree-sitter"))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow!("tree-sitter could not parse {path}"))?;
    let root = tree.root_node();

    let definitions = prune(match tags {
        Some(query) => from_tags(query, root, source),
        None => from_heuristic(root, source),
    });
    let mut units = build_units(&definitions, path, language, source);
    units.sort_by(|a, b| {
        (a.start_line, std::cmp::Reverse(a.end_line), a.kind, &a.name).cmp(&(
            b.start_line,
            std::cmp::Reverse(b.end_line),
            b.kind,
            &b.name,
        ))
    });
    let comments = comments(root, source);
    let filler_lines = filler_lines(source, &comments);
    Ok(ParsedFile {
        units,
        comments,
        filler_lines,
    })
}

/// What a definition is before nesting decides class-member vs free function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DefKind {
    /// class, struct, interface, trait, enum, object, impl, type.
    Container,
    /// module / namespace / package: a class unit, but functions in it stay
    /// functions (a Rust `mod tests` does not turn its fns into methods).
    Module,
    Method,
    Function,
}

impl DefKind {
    /// When one node is captured twice (Rust tags mark an impl fn both
    /// `definition.method` and `definition.function`), the most specific wins.
    fn priority(self) -> u8 {
        match self {
            DefKind::Method => 3,
            DefKind::Function => 2,
            DefKind::Container => 1,
            DefKind::Module => 0,
        }
    }

    fn is_class_like(self) -> bool {
        matches!(self, DefKind::Container | DefKind::Module)
    }
}

struct Definition<'t> {
    node: Node<'t>,
    kind: DefKind,
    name: String,
}

fn tag_kind(capture: &str) -> Option<DefKind> {
    let kind = capture.strip_prefix("definition.")?;
    Some(match kind {
        "class" | "interface" | "struct" | "enum" | "trait" | "object" | "type" | "union"
        | "impl" => DefKind::Container,
        "module" | "namespace" | "package" => DefKind::Module,
        "method" => DefKind::Method,
        "function" => DefKind::Function,
        _ => return None,
    })
}

fn from_tags<'t>(query: &Query, root: Node<'t>, source: &str) -> Vec<Definition<'t>> {
    let names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, root, source.as_bytes());
    let mut by_node: HashMap<usize, Definition<'t>> = HashMap::new();
    while let Some(found) = matches.next() {
        let mut definition = None;
        let mut name = None;
        let mut member = None;
        for capture in found.captures {
            let capture_name = names[capture.index as usize];
            if capture_name == "name" {
                name = Some(capture.node);
            } else if capture_name == "name.member" {
                member = Some(capture.node);
            } else if let Some(kind) = tag_kind(capture_name) {
                definition = Some((capture.node, kind));
            }
        }
        let Some((captured, kind)) = definition else {
            continue;
        };
        let Some(node) = definition_node(captured, kind) else {
            continue;
        };
        let mut name = name
            .map(|node| qualified_name(node, source))
            .unwrap_or_else(|| node_name(node, source));
        // `@name.member` is a lintent extension: `const loadFn = x.handler(…)`
        // is named `loadFn.handler`.
        if let Some(member) = member {
            name = format!("{name}.{}", text(member, source));
        }
        let candidate = Definition { node, kind, name };
        match by_node.get(&node.id()) {
            Some(existing) if existing.kind.priority() >= kind.priority() => {}
            _ => {
                by_node.insert(node.id(), candidate);
            }
        }
    }
    by_node.into_values().collect()
}

/// C-family tags queries capture the *declarator* (`add(int a, int b)`), not
/// the definition with its body. Climb to the enclosing definition; a
/// function whose declarator sits in a plain declaration is a prototype and
/// has no body to judge. Signatures (TS overloads, `declare function`,
/// abstract and interface methods) have no body either.
fn definition_node(node: Node, kind: DefKind) -> Option<Node> {
    let mut node = node;
    if node.kind().contains("declarator") {
        while node.kind().contains("declarator") {
            node = node.parent()?;
        }
        let callable = matches!(kind, DefKind::Function | DefKind::Method);
        if callable
            && matches!(
                node.kind(),
                "declaration" | "field_declaration" | "parameter_declaration"
            )
        {
            return None;
        }
    }
    if matches!(kind, DefKind::Function | DefKind::Method) && node.kind().contains("signature") {
        return None;
    }
    Some(node)
}

/// `Foo::baz` (an out-of-line C++ method) is named `Foo.baz`.
fn qualified_name(name: Node, source: &str) -> String {
    let own = text(name, source).to_string();
    let Some(parent) = name.parent() else {
        return own;
    };
    match (
        parent.child_by_field_name("scope"),
        parent.child_by_field_name("name"),
    ) {
        (Some(scope), Some(field)) if field == name => format!("{}.{own}", text(scope, source)),
        _ => own,
    }
}

/// Drops definitions that would duplicate another unit: a function that is
/// the value of another definition (`export const h = async function named() {}`
/// captured both as `h` and as `named`), and any two with the same span.
fn prune(definitions: Vec<Definition<'_>>) -> Vec<Definition<'_>> {
    let callable = |kind: DefKind| matches!(kind, DefKind::Function | DefKind::Method);
    let callables: std::collections::HashSet<usize> = definitions
        .iter()
        .filter(|d| callable(d.kind))
        .map(|d| d.node.id())
        .collect();
    // Is `node` the value of another callable definition, with no body or
    // block in between (which would make it a genuinely nested function)?
    let is_value_of_callable = |node: Node| {
        let mut current = node.parent();
        while let Some(ancestor) = current {
            if callables.contains(&ancestor.id()) {
                return true;
            }
            let kind = ancestor.kind();
            if kind.contains("block") || kind.contains("body") || ancestor.parent().is_none() {
                return false;
            }
            current = ancestor.parent();
        }
        false
    };
    let mut kept: Vec<Definition> = definitions
        .into_iter()
        .filter(|definition| !(callable(definition.kind) && is_value_of_callable(definition.node)))
        .collect();
    kept.sort_by_key(|definition| std::cmp::Reverse(definition.kind.priority()));
    let mut spans = std::collections::HashSet::new();
    kept.retain(|definition| {
        let (first, last) = anchor(definition.node);
        spans.insert((first.start_byte(), last.end_byte()))
    });
    kept
}

/// `fun` alone would also match `function`, which is fine: both are functions.
const FUNCTION_WORDS: &[&str] = &["function", "method", "func", "fun", "procedure", "sub"];
const FUNCTION_SUFFIXES: &[&str] = &["declaration", "definition", "item"];
const CLASS_WORDS: &[&str] = &[
    "class",
    "struct",
    "interface",
    "trait",
    "impl",
    "enum",
    "object",
    "module",
];
const CLASS_SUFFIXES: &[&str] = &["declaration", "definition", "item", "specifier"];

/// Heuristic classification of a node type (see the module docs).
fn classify_node_type(kind: &str) -> Option<DefKind> {
    let ends_with_word = |word: &str, suffix: &str, joiners: &[&str]| {
        joiners.iter().any(|joiner| {
            let tail = format!("{word}{joiner}{suffix}");
            kind.strip_suffix(&tail)
                .is_some_and(|head| head.is_empty() || head.ends_with('_'))
        })
    };
    for word in FUNCTION_WORDS {
        for suffix in FUNCTION_SUFFIXES {
            if ends_with_word(word, suffix, &["_", ""]) {
                return Some(if *word == "method" {
                    DefKind::Method
                } else {
                    DefKind::Function
                });
            }
        }
    }
    for word in CLASS_WORDS {
        for suffix in CLASS_SUFFIXES {
            if ends_with_word(word, suffix, &["_"]) {
                return Some(if *word == "module" {
                    DefKind::Module
                } else {
                    DefKind::Container
                });
            }
        }
    }
    None
}

fn from_heuristic<'t>(root: Node<'t>, source: &str) -> Vec<Definition<'t>> {
    let mut found = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.is_named() {
            if let Some(kind) = classify_node_type(node.kind()) {
                found.push(Definition {
                    node,
                    kind,
                    name: node_name(node, source),
                });
            }
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    found
}

/// Answers "which definition encloses this node?" by walking parents.
struct Nesting<'a, 't> {
    definitions: &'a [Definition<'t>],
    index: HashMap<usize, usize>,
}

impl<'a, 't> Nesting<'a, 't> {
    fn new(definitions: &'a [Definition<'t>]) -> Self {
        let index = definitions
            .iter()
            .enumerate()
            .map(|(position, definition)| (definition.node.id(), position))
            .collect();
        Nesting { definitions, index }
    }

    /// The kind of the nearest enclosing scope: a definition, or a node
    /// whose type the heuristic recognises (so a tags query that only marks
    /// functions still sees the class around them).
    fn enclosing_kind(&self, node: Node<'t>) -> Option<DefKind> {
        let mut current = node.parent();
        while let Some(ancestor) = current {
            if let Some(&position) = self.index.get(&ancestor.id()) {
                return Some(self.definitions[position].kind);
            }
            if ancestor.is_named() {
                if let Some(kind) = classify_node_type(ancestor.kind()) {
                    return Some(kind);
                }
            }
            current = ancestor.parent();
        }
        None
    }

    /// The class a method belongs to: the nearest ancestor that is either a
    /// class-like definition or a node whose type looks like a class (a Rust
    /// `impl` is not a tags definition but is the obvious parent); else, for
    /// receiver-style methods (Go), the receiver's type name with no parent
    /// source, since the type is declared elsewhere.
    fn container(&self, node: Node<'t>, source: &str) -> Option<Container<'t>> {
        let mut current = node.parent();
        while let Some(ancestor) = current {
            // A function in between ends the search: an object-literal
            // method inside a class method does not belong to that class.
            if let Some(&position) = self.index.get(&ancestor.id()) {
                let definition = &self.definitions[position];
                if !definition.kind.is_class_like() {
                    return None;
                }
                return Some(Container {
                    name: definition.name.clone(),
                    node: Some(definition.node),
                    is_module: definition.kind == DefKind::Module,
                });
            }
            if ancestor.is_named() {
                match classify_node_type(ancestor.kind()) {
                    Some(DefKind::Container) => {
                        return Some(Container {
                            name: node_name(ancestor, source),
                            node: Some(ancestor),
                            is_module: false,
                        })
                    }
                    Some(DefKind::Function | DefKind::Method) => return None,
                    _ => {}
                }
            }
            current = ancestor.parent();
        }
        let receiver = node.child_by_field_name("receiver")?;
        let type_name = first_descendant(receiver, |n| n.kind().contains("type_identifier"))?;
        Some(Container {
            name: text(type_name, source).to_string(),
            node: None,
            is_module: false,
        })
    }
}

struct Container<'t> {
    name: String,
    /// Source for `parentSource`; `None` when the type lives elsewhere.
    node: Option<Node<'t>>,
    is_module: bool,
}

fn build_units(definitions: &[Definition], path: &str, language: &str, source: &str) -> Vec<Unit> {
    let nesting = Nesting::new(definitions);
    definitions
        .iter()
        .map(|definition| {
            let nearest = nesting.enclosing_kind(definition.node);
            let container = nesting.container(definition.node, source);
            let member_of_class = container.as_ref().is_some_and(|c| !c.is_module);
            // Tags queries sometimes call anything in a body list a method (a
            // Rust fn in `mod tests`); a method needs a class, not a module.
            let kind = match definition.kind {
                DefKind::Container | DefKind::Module => ScopeKind::Class,
                DefKind::Method if container.as_ref().is_some_and(|c| c.is_module) => {
                    ScopeKind::Function
                }
                DefKind::Method => ScopeKind::Method,
                DefKind::Function if nearest == Some(DefKind::Container) => ScopeKind::Method,
                DefKind::Function => ScopeKind::Function,
            };
            let own_name = match definition.node.parent() {
                Some(parent)
                    if definition.name == "<anonymous>" && parent.kind() == "export_statement" =>
                {
                    "default".to_string()
                }
                _ => definition.name.clone(),
            };
            let (name, parent) = match container {
                Some(container) if kind == ScopeKind::Method && member_of_class => {
                    (format!("{}.{own_name}", container.name), container.node)
                }
                _ => (own_name, None),
            };
            let (first, last) = anchor(definition.node);
            let (text, truncated) = truncate_source(&source[first.start_byte()..last.end_byte()]);
            Unit {
                kind,
                name,
                language: language.to_string(),
                path: path.to_string(),
                start_line: first.start_position().row + 1,
                end_line: last.end_position().row + 1,
                source: text,
                parent_source: parent.map(|node| {
                    let (first, last) = anchor(node);
                    truncate_chars(
                        &source[first.start_byte()..last.end_byte()],
                        MAX_PARENT_CHARS,
                    )
                }),
                truncated,
            }
        })
        .collect()
}

/// A definition's display name: the `name` field, else the name inside a
/// `declarator` (C-family), else the implemented type of an impl (`type`
/// field, verbatim, so `impl T for u8` is `u8`), else the first
/// identifier-like child.
fn node_name(node: Node, source: &str) -> String {
    if node.child_by_field_name("name").is_none()
        && node.child_by_field_name("declarator").is_none()
    {
        if let Some(ty) = node.child_by_field_name("type") {
            return text(ty, source).to_string();
        }
    }
    for field in ["name", "declarator"] {
        if let Some(child) = node.child_by_field_name(field) {
            if let Some(identifier) = first_descendant(child, is_identifier_like) {
                return text(identifier, source).to_string();
            }
        }
    }
    let mut cursor = node.walk();
    let found = node
        .named_children(&mut cursor)
        .find(|child| is_identifier_like(*child))
        .map(|child| text(child, source).to_string());
    found.unwrap_or_else(|| "<anonymous>".to_string())
}

fn is_identifier_like(node: Node) -> bool {
    let kind = node.kind();
    kind.contains("identifier") || kind == "name" || kind == "constant" || kind == "word"
}

/// Breadth-first, so the shallowest match wins (the function's own name, not
/// a parameter's).
fn first_descendant<'t>(node: Node<'t>, matches: impl Fn(Node<'t>) -> bool) -> Option<Node<'t>> {
    let mut queue = std::collections::VecDeque::from([node]);
    while let Some(current) = queue.pop_front() {
        if matches(current) {
            return Some(current);
        }
        let mut cursor = current.walk();
        queue.extend(current.named_children(&mut cursor));
    }
    None
}

/// The first and last node of what a reader considers the definition.
fn anchor(node: Node) -> (Node, Node) {
    let mut outer = node;
    while let Some(parent) = outer.parent() {
        let is_root = parent.parent().is_none();
        let kind = parent.kind();
        let wrapper_kind = ["_statement", "_declaration", "_definition"]
            .iter()
            .any(|suffix| kind.ends_with(suffix));
        if is_root || !wrapper_kind || substantive_children(parent) != 1 {
            break;
        }
        outer = parent;
    }
    let mut first = outer;
    while let Some(previous) = first.prev_named_sibling() {
        if !is_decoration(previous) {
            break;
        }
        first = previous;
    }
    (first, outer)
}

fn is_decoration(node: Node) -> bool {
    let kind = node.kind();
    kind.contains("decorator") || kind.contains("attribute") || kind.contains("annotation")
}

fn substantive_children(node: Node) -> usize {
    let mut cursor = node.walk();
    let count = node
        .named_children(&mut cursor)
        .filter(|child| !is_decoration(*child) && !child.kind().contains("comment"))
        .count();
    count
}

fn comments(root: Node, source: &str) -> Vec<CommentLine> {
    let mut lines = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind().contains("comment") {
            // Don't descend: a doc comment's inner nodes are the same text.
            let start = node.start_position().row + 1;
            for (offset, line) in text(node, source).lines().enumerate() {
                lines.push(CommentLine {
                    line: start + offset,
                    text: line.to_string(),
                });
            }
            continue;
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    lines.sort_by_key(|comment| comment.line);
    lines
}

fn filler_lines(source: &str, comments: &[CommentLine]) -> std::collections::BTreeSet<usize> {
    let whole_line_comments: std::collections::HashMap<usize, &str> =
        comments.iter().map(|c| (c.line, c.text.trim())).collect();
    source
        .lines()
        .enumerate()
        .filter(|(index, line)| {
            let line = line.trim();
            line.is_empty()
                || whole_line_comments
                    .get(&(index + 1))
                    .is_some_and(|c| *c == line)
        })
        .map(|(index, _)| index + 1)
        .collect()
}

fn truncate_source(text: &str) -> (String, bool) {
    let total_lines = text.lines().count();
    if total_lines <= MAX_SOURCE_LINES && text.len() <= MAX_SOURCE_CHARS {
        return (text.to_string(), false);
    }
    let mut kept = String::new();
    let mut shown = 0;
    for line in text.lines().take(MAX_SOURCE_LINES) {
        if kept.len() + line.len() + 1 > MAX_SOURCE_CHARS {
            break;
        }
        kept.push_str(line);
        kept.push('\n');
        shown += 1;
    }
    if shown == 0 {
        // The first line alone is over the limit (minified code): send a
        // prefix of it rather than an empty body.
        kept = truncate_chars(text, MAX_SOURCE_CHARS);
        kept.push('\n');
    }
    kept.push_str(&format!(
        "... [lintent: scope truncated after {shown} lines; {} more lines not shown]",
        total_lines - shown
    ));
    (kept, true)
}

fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((cut, _)) => text[..cut].to_string(),
        None => text.to_string(),
    }
}

fn text<'s>(node: Node, source: &'s str) -> &'s str {
    &source[node.byte_range()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::languages::Registry;

    fn units(registry: &Registry, language: &str, source: &str) -> Vec<Unit> {
        let spec = registry.get(language).unwrap();
        parse_file("src/file", spec, source).unwrap().units
    }

    fn heuristic_units(registry: &Registry, language: &str, source: &str) -> Vec<Unit> {
        let loaded = registry.get(language).unwrap().load().unwrap();
        parse_with("src/file", language, &loaded.grammar, None, source)
            .unwrap()
            .units
    }

    fn summary(units: &[Unit]) -> Vec<String> {
        units
            .iter()
            .map(|u| format!("{}:{}-{} {}", u.kind, u.start_line, u.end_line, u.name))
            .collect()
    }

    const TS: &str = r#"import x from "y";

@Injectable()
export class UserService {
  private repo = new Repo();

  constructor(private readonly db: Db) {}

  @Get()
  async find(id: string) {
    const helper = () => id;
    return this.repo.find(helper());
  }

  handle = async (event: Event) => {
    return event;
  };
}

export interface Shape { area(): number }

export function plain(a: number) {
  return a + 1;
}

export const arrow = (n: number) => n * 2;

[1, 2].map((n) => n + 1);
"#;

    #[test]
    fn typescript_via_tags() {
        let registry = Registry::builtin();
        let found = units(&registry, "typescript", TS);
        assert_eq!(
            summary(&found),
            vec![
                "class:3-18 UserService",
                "method:7-7 UserService.constructor",
                "method:9-13 UserService.find",
                "function:11-11 helper",
                "method:15-17 UserService.handle",
                "class:20-20 Shape",
                "function:22-24 plain",
                "function:26-26 arrow",
            ]
        );
        let find = found.iter().find(|u| u.name == "UserService.find").unwrap();
        assert!(find.source.starts_with("@Get()"), "{}", find.source);
        assert!(find
            .parent_source
            .as_deref()
            .unwrap()
            .starts_with("@Injectable()\nexport class UserService"));
        let plain = found.iter().find(|u| u.name == "plain").unwrap();
        assert!(plain.source.starts_with("export function plain"));
        assert!(plain.parent_source.is_none());
        let arrow = found.iter().find(|u| u.name == "arrow").unwrap();
        assert!(arrow.source.starts_with("export const arrow"));
    }

    #[test]
    fn functions_passed_to_a_bound_call_are_scopes_and_collection_callbacks_are_not() {
        let source = "// keep R1c -- the gate\nconst loadAuthGateFn = createServerFn().handler(async (): Promise<AuthGate> => {\n  const auth = await authenticateRequest(getRequest());\n  return { auth };\n});\n\nexport const reconcileOrgFn = createServerFn({ method: \"POST\" })\n  .validator((data: unknown) => parse(data))\n  .handler(async ({ data }) => {\n    return data;\n  });\n\nconst onClick = useCallback(() => {\n  go();\n}, []);\n\nconst doubled = items.map((n) => n * 2);\nitems.forEach((n) => log(n));\n";
        let registry = Registry::builtin();
        for language in ["typescript", "tsx", "javascript"] {
            let source = if language == "javascript" {
                source
                    .replace(": Promise<AuthGate>", "")
                    .replace(": unknown", "")
            } else {
                source.to_string()
            };
            let found = units(&registry, language, &source);
            assert_eq!(
                summary(&found),
                vec![
                    "function:2-5 loadAuthGateFn.handler",
                    "function:7-11 reconcileOrgFn.handler",
                    "function:13-15 onClick",
                ],
                "{language}"
            );
            assert!(found[1]
                .source
                .starts_with("export const reconcileOrgFn = createServerFn"));
        }
    }

    #[test]
    fn c_and_cpp_units_are_definitions_with_bodies() {
        let registry = Registry::builtin();
        let c = "struct Point { int x; };\nint sub(int a, int b);\nint add(int a, int b) {\n  return a + b;\n}\n";
        let found = units(&registry, "c", c);
        assert_eq!(summary(&found), vec!["class:1-1 Point", "function:3-5 add"]);
        assert!(
            found[1].source.starts_with("int add(int a, int b) {")
                && found[1].source.ends_with('}')
        );

        let cpp = "class Foo {\n public:\n  void bar() { run(); }\n  void decl();\n};\n\nvoid Foo::baz() {\n  run();\n}\n";
        let found = units(&registry, "cpp", cpp);
        assert_eq!(
            summary(&found),
            vec!["class:1-5 Foo", "method:3-3 Foo.bar", "method:7-9 Foo.baz"]
        );
    }

    #[test]
    fn typescript_bodiless_duplicate_default_and_nested_object_methods() {
        let source = "export function over(a: string): void;\nexport function over(a: unknown) {}\ndeclare function ambient(): void;\nabstract class A {\n  abstract run(): void;\n  go() {\n    const o = { foo() { return 1; } };\n  }\n}\ninterface I { m(): void }\nexport const handler = async function named() {};\nexport default function () {}\n";
        let registry = Registry::builtin();
        let found = units(&registry, "typescript", source);
        assert_eq!(
            summary(&found),
            vec![
                "function:2-2 over",
                "class:4-9 A",
                "method:6-8 A.go",
                "method:7-7 foo",
                "class:10-10 I",
                "function:11-11 handler",
                "function:12-12 default",
            ]
        );
        let foo = found.iter().find(|u| u.name == "foo").unwrap();
        assert!(
            foo.parent_source.is_none(),
            "an object literal in a method is not the class"
        );
    }

    #[test]
    fn rust_impl_for_a_primitive_is_named_after_the_type() {
        let source = "trait T { fn f(&self); }\nimpl T for u8 {\n    fn f(&self) {}\n}\n";
        let registry = Registry::builtin();
        let found = units(&registry, "rust", source);
        assert!(
            found.iter().any(|u| u.name == "u8.f"),
            "{:?}",
            summary(&found)
        );
    }

    #[test]
    fn scala_packages_and_enum_cases_are_not_classes() {
        let source = "package shop\n\nenum Color {\n  case Red, Green\n  case Mix(a: Int)\n}\n\nclass Cart {\n  def total(): Int = 1\n}\n";
        let registry = Registry::builtin();
        let names: Vec<String> = units(&registry, "scala", source)
            .into_iter()
            .map(|u| u.name)
            .collect();
        assert_eq!(names, vec!["Color", "Cart", "Cart.total"]);
    }

    #[test]
    fn python_via_tags_promotes_class_functions_to_methods() {
        let source = "class Repo:\n    @staticmethod\n    def build():\n        def inner():\n            pass\n        return Repo()\n\n@cache\ndef top():\n    return 1\n";
        let registry = Registry::builtin();
        let found = units(&registry, "python", source);
        assert_eq!(
            summary(&found),
            vec![
                "class:1-6 Repo",
                "method:2-6 Repo.build",
                "function:4-5 inner",
                "function:8-10 top",
            ]
        );
        assert!(found[1]
            .parent_source
            .as_deref()
            .unwrap()
            .starts_with("class Repo:"));
    }

    #[test]
    fn rust_via_tags_uses_impl_as_parent() {
        let source = "#[derive(Debug)]\npub struct Point { x: i32 }\n\nimpl Point {\n    pub fn new(x: i32) -> Self { Point { x } }\n}\n\nmod tests {\n    #[test]\n    fn works() {}\n}\n";
        let registry = Registry::builtin();
        let found = units(&registry, "rust", source);
        assert_eq!(
            summary(&found),
            vec![
                "class:1-2 Point",
                "method:5-5 Point.new",
                "class:8-11 tests",
                "function:9-10 works",
            ]
        );
        assert!(found[1]
            .parent_source
            .as_deref()
            .unwrap()
            .starts_with("impl Point {"));
    }

    #[test]
    fn go_via_tags_names_methods_by_receiver() {
        let source = "package store\n\ntype Store struct {\n\titems map[string]int\n}\n\nfunc (s *Store) Get(key string) int {\n\treturn s.items[key]\n}\n\nfunc New() *Store { return &Store{} }\n";
        let registry = Registry::builtin();
        let found = units(&registry, "go", source);
        assert_eq!(
            summary(&found),
            vec![
                "class:3-5 Store",
                "method:7-9 Store.Get",
                "function:11-11 New"
            ]
        );
    }

    #[test]
    fn java_via_tags() {
        let source = "package a;\n\n@Service\npublic class Billing {\n  @Override\n  public int total(int x) {\n    return x;\n  }\n}\n";
        let registry = Registry::builtin();
        let found = units(&registry, "java", source);
        assert_eq!(
            summary(&found),
            vec!["class:3-9 Billing", "method:5-8 Billing.total"]
        );
    }

    #[test]
    fn kotlin_has_no_tags_query_and_uses_the_heuristic() {
        let source =
            "class Cart {\n  fun total(): Int {\n    return 1\n  }\n}\n\nfun main() {\n}\n";
        let registry = Registry::builtin();
        assert_eq!(registry.get("kotlin").unwrap().strategy(), "heuristic");
        let found = units(&registry, "kotlin", source);
        assert_eq!(
            summary(&found),
            vec![
                "class:1-5 Cart",
                "method:2-4 Cart.total",
                "function:7-8 main"
            ]
        );
    }

    #[test]
    fn heuristic_on_a_tagged_grammar() {
        let source = "export class A {\n  run() { return 1; }\n}\nfunction free() {}\n";
        let registry = Registry::builtin();
        let found = heuristic_units(&registry, "typescript", source);
        assert_eq!(
            summary(&found),
            vec!["class:1-3 A", "method:2-2 A.run", "function:4-4 free"]
        );
    }

    #[test]
    fn heuristic_node_type_classification() {
        assert_eq!(
            classify_node_type("function_definition"),
            Some(DefKind::Function)
        );
        assert_eq!(
            classify_node_type("generator_function_declaration"),
            Some(DefKind::Function)
        );
        assert_eq!(
            classify_node_type("method_declaration"),
            Some(DefKind::Method)
        );
        assert_eq!(classify_node_type("function_item"), Some(DefKind::Function));
        assert_eq!(
            classify_node_type("abstract_class_declaration"),
            Some(DefKind::Container)
        );
        assert_eq!(
            classify_node_type("struct_specifier"),
            Some(DefKind::Container)
        );
        assert_eq!(classify_node_type("impl_item"), Some(DefKind::Container));
        assert_eq!(
            classify_node_type("module_definition"),
            Some(DefKind::Module)
        );
        assert_eq!(classify_node_type("function_declarator"), None);
        assert_eq!(classify_node_type("function_signature_item"), None);
        assert_eq!(classify_node_type("refunction_item"), None);
    }

    #[test]
    fn comments_are_collected_in_any_language() {
        let registry = Registry::builtin();
        let spec = registry.get("python").unwrap();
        let parsed = parse_file(
            "a.py",
            spec,
            "x = 1  # first\n\"\"\"not a comment\"\"\"\n# second\n",
        )
        .unwrap();
        let lines: Vec<_> = parsed
            .comments
            .iter()
            .map(|c| (c.line, c.text.as_str()))
            .collect();
        assert_eq!(lines, vec![(1, "# first"), (3, "# second")]);
    }

    #[test]
    fn long_scopes_are_truncated_with_a_marker() {
        let body: String = (0..450).map(|i| format!("  const v{i} = {i};\n")).collect();
        let source = format!("function big() {{\n{body}}}\n");
        let registry = Registry::builtin();
        let found = units(&registry, "typescript", &source);
        let unit = &found[0];
        assert!(unit.truncated);
        assert_eq!(unit.end_line, 452);
        assert_eq!(unit.source.lines().count(), MAX_SOURCE_LINES + 1);
        assert!(unit.source.ends_with("52 more lines not shown]"));
    }

    #[test]
    fn very_long_lines_are_truncated_by_size() {
        let long = "x".repeat(MAX_SOURCE_CHARS);
        let (text, truncated) = truncate_source(&format!("a\n{long}\nb\n"));
        assert!(truncated);
        assert!(text.len() < MAX_SOURCE_CHARS);
        assert!(text.ends_with("2 more lines not shown]"));
    }

    #[test]
    fn a_single_over_long_line_keeps_a_prefix() {
        let line = "y".repeat(MAX_SOURCE_CHARS + 10);
        let (text, truncated) = truncate_source(&format!("{line}\nz\n"));
        assert!(truncated);
        assert!(text.starts_with("yyyy"));
        assert!(text.len() > MAX_SOURCE_CHARS && text.len() < MAX_SOURCE_CHARS + 200);
    }

    #[test]
    fn parent_source_is_capped() {
        let filler: String = (0..400).map(|i| format!("  f{i} = {i};\n")).collect();
        let source = format!("class Big {{\n{filler}  run() {{ return 1; }}\n}}\n");
        let registry = Registry::builtin();
        let found = units(&registry, "typescript", &source);
        let method = found.iter().find(|u| u.kind == ScopeKind::Method).unwrap();
        assert_eq!(
            method.parent_source.as_ref().unwrap().chars().count(),
            MAX_PARENT_CHARS
        );
    }

    #[test]
    fn scope_kind_parses() {
        assert_eq!("method".parse::<ScopeKind>(), Ok(ScopeKind::Method));
        assert!("struct".parse::<ScopeKind>().is_err());
    }
}
