use super::{Builder, Parsed, SourceFile, children, descendants, field, literal, route, text};
use std::collections::{BTreeMap, BTreeSet};
use tree_sitter::Node;

struct Class<'a> {
    file: &'a SourceFile,
    name: &'a str,
    qualified: String,
    imports: BTreeSet<&'a str>,
    declaration: Node<'a>,
    methods: Vec<Method<'a>>,
    fields: BTreeMap<&'a str, &'a str>,
    repository: Option<(String, &'a str)>,
}

struct Method<'a> {
    node: Node<'a>,
    name: &'a str,
    arity: usize,
    graph_id: Option<String>,
}

pub(super) fn collect<'a>(parsed: &'a [Parsed<'a>], files: &[SourceFile], graph: &mut Builder) {
    let mut classes = Vec::new();
    for entry in parsed {
        let file = entry.file;
        let root = entry.tree.root_node();
        let package = children(root)
            .into_iter()
            .find(|node| node.kind() == "package_declaration")
            .and_then(|node| node.named_child(0))
            .map_or("", |node| text(node, file));
        let imports = children(root)
            .into_iter()
            .filter(|node| node.kind() == "import_declaration")
            .filter_map(|node| node.named_child(0))
            .map(|node| text(node, file))
            .collect::<BTreeSet<_>>();
        for node in descendants(root).into_iter().filter(|node| {
            matches!(
                node.kind(),
                "class_declaration" | "interface_declaration" | "record_declaration"
            )
        }) {
            let Some(name) = field(node, "name", file) else {
                continue;
            };
            let Some(body) = node.child_by_field_name("body") else {
                continue;
            };
            let qualified = if package.is_empty() {
                name.to_owned()
            } else {
                format!("{package}.{name}")
            };
            let controller = annotation(node, file, "RestController").is_some()
                || annotation(node, file, "Controller").is_some();
            let service = annotation(node, file, "Service").is_some();
            let base = annotation(node, file, "RequestMapping")
                .and_then(|item| mapping_path(item, file))
                .unwrap_or_default();
            let mut fields = BTreeMap::new();
            let mut methods = Vec::new();
            for member in children(body) {
                if member.kind() == "field_declaration" {
                    let Some(ty) = field(member, "type", file) else {
                        continue;
                    };
                    for variable in children(member)
                        .into_iter()
                        .filter(|item| item.kind() == "variable_declarator")
                    {
                        if let Some(name) = field(variable, "name", file) {
                            fields.insert(name, ty);
                        }
                    }
                }
                if member.kind() != "method_declaration" {
                    continue;
                }
                let Some(method_name) = field(member, "name", file) else {
                    continue;
                };
                let Some(parameters) = member.child_by_field_name("parameters") else {
                    continue;
                };
                let key = format!("{qualified}.{method_name}{}", text(parameters, file));
                let graph_id = if controller {
                    endpoint(member, file, base).map(|route| {
                        let label = format!("{} {}", route.method, route.path_template);
                        let id = graph.node(
                            "ENDPOINT",
                            file,
                            member.start_position().row + 1,
                            &key,
                            &label,
                        );
                        graph.route(&id, Some(route));
                        id
                    })
                } else if service {
                    Some(graph.node(
                        "SERVICE",
                        file,
                        member.start_position().row + 1,
                        &key,
                        &format!("{name}.{method_name}"),
                    ))
                } else {
                    None
                };
                methods.push(Method {
                    node: member,
                    name: method_name,
                    arity: children(parameters)
                        .iter()
                        .filter(|param| {
                            matches!(param.kind(), "formal_parameter" | "spread_parameter")
                        })
                        .count(),
                    graph_id,
                });
            }
            let repository = repository_entity(node, body, file).map(|entity| {
                (
                    graph.node(
                        "REPOSITORY",
                        file,
                        node.start_position().row + 1,
                        &qualified,
                        name,
                    ),
                    entity,
                )
            });
            classes.push(Class {
                file,
                name,
                qualified,
                imports: imports.clone(),
                declaration: node,
                methods,
                fields,
                repository,
            });
        }
    }
    let tables = migration_tables(files);
    for class in &classes {
        for imported in &class.imports {
            if let Some(target) = classes
                .iter()
                .find(|candidate| candidate.qualified == *imported)
            {
                graph.import(class.file, target.file);
            }
        }
        if let Some((repository, entity)) = &class.repository
            && let Some(entity) = resolve_type(&classes, class, entity)
            && let Some((schema, name)) = table_mapping(entity)
            && let Some((file, line)) = tables.get(&(schema.clone(), name.clone()))
        {
            let id = graph.node("DB_TABLE", file, *line, &format!("{schema}.{name}"), &name);
            graph.schema(&id, schema);
            graph.edge(repository, &id, "USES_TABLE");
        }
        for method in &class.methods {
            if let Some(id) = &method.graph_id {
                let mut traversal = Calls {
                    classes: &classes,
                    class,
                    graph,
                    source: id,
                    visited: BTreeSet::new(),
                };
                traversal.walk(method, 0);
            }
        }
    }
}

struct Calls<'a, 'g> {
    classes: &'a [Class<'a>],
    class: &'a Class<'a>,
    graph: &'g mut Builder,
    source: &'g str,
    visited: BTreeSet<usize>,
}

impl Calls<'_, '_> {
    fn walk(&mut self, method: &Method<'_>, depth: usize) {
        if depth > 6 || !self.visited.insert(method.node.start_byte()) {
            return;
        }
        let Some(body) = method.node.child_by_field_name("body") else {
            return;
        };
        let file = self.class.file;
        for call in descendants(body)
            .into_iter()
            .filter(|node| node.kind() == "method_invocation")
        {
            let Some(name) = field(call, "name", file) else {
                continue;
            };
            let arity = call.child_by_field_name("arguments").map_or(0, |node| {
                children(node)
                    .iter()
                    .filter(|child| {
                        child.kind() != "line_comment" && child.kind() != "block_comment"
                    })
                    .count()
            });
            if let Some(object) = field(call, "object", file).filter(|object| *object != "this") {
                let field_name = object.strip_prefix("this.").unwrap_or(object);
                let Some(target) = self
                    .class
                    .fields
                    .get(field_name)
                    .and_then(|ty| resolve_type(self.classes, self.class, ty))
                else {
                    continue;
                };
                if let Some((repository, _)) = &target.repository {
                    self.graph.edge(self.source, repository, "CALLS");
                } else if let Some(id) =
                    method_named(target, name, arity).and_then(|method| method.graph_id.as_ref())
                {
                    self.graph.edge(self.source, id, "CALLS");
                }
            } else if let Some(local) = method_named(self.class, name, arity) {
                self.walk(local, depth + 1);
            }
        }
    }
}

fn method_named<'a, 'b>(class: &'a Class<'b>, name: &str, arity: usize) -> Option<&'a Method<'b>> {
    let mut matching = class
        .methods
        .iter()
        .filter(|method| method.name == name && method.arity == arity);
    let method = matching.next()?;
    matching.next().is_none().then_some(method)
}

fn resolve_type<'a, 'b>(
    classes: &'a [Class<'b>],
    owner: &Class<'_>,
    name: &str,
) -> Option<&'a Class<'b>> {
    if name.contains('.') {
        return classes.iter().find(|candidate| candidate.qualified == name);
    }
    let package = owner
        .qualified
        .rsplit_once('.')
        .map_or("", |(package, _)| package);
    let local = if package.is_empty() {
        name.to_owned()
    } else {
        format!("{package}.{name}")
    };
    let mut matches = classes.iter().filter(|candidate| {
        candidate.name == name
            && (candidate.qualified == local
                || owner.imports.contains(candidate.qualified.as_str()))
    });
    let found = matches.next()?;
    matches.next().is_none().then_some(found)
}

fn repository_entity<'a>(node: Node<'_>, body: Node<'_>, file: &'a SourceFile) -> Option<&'a str> {
    for generic in descendants(node)
        .into_iter()
        .filter(|item| item.start_byte() < body.start_byte() && item.kind() == "generic_type")
    {
        let name = text(generic.named_child(0)?, file);
        if name.rsplit('.').next() == Some("JpaRepository") {
            let entity = generic.named_child(1)?.named_child(0)?;
            return Some(text(entity, file));
        }
    }
    None
}

fn annotation<'a>(node: Node<'a>, file: &SourceFile, name: &str) -> Option<Node<'a>> {
    let modifiers = children(node)
        .into_iter()
        .find(|child| child.kind() == "modifiers")?;
    children(modifiers).into_iter().find(|annotation| {
        matches!(annotation.kind(), "annotation" | "marker_annotation")
            && field(*annotation, "name", file).and_then(|name| name.rsplit('.').next())
                == Some(name)
    })
}

fn endpoint(node: Node<'_>, file: &SourceFile, base: &str) -> Option<super::Route> {
    for (annotation_name, verb) in [
        ("GetMapping", "GET"),
        ("PostMapping", "POST"),
        ("PutMapping", "PUT"),
        ("PatchMapping", "PATCH"),
        ("DeleteMapping", "DELETE"),
    ] {
        if let Some(annotation) = annotation(node, file, annotation_name) {
            let relative = mapping_path(annotation, file)?;
            let path = if relative.is_empty() {
                base.to_owned()
            } else {
                format!(
                    "{}/{}",
                    base.trim_end_matches('/'),
                    relative.trim_start_matches('/')
                )
            };
            return route(verb, &path);
        }
    }
    None
}

fn mapping_path<'a>(node: Node<'_>, file: &'a SourceFile) -> Option<&'a str> {
    let Some(args) = node.child_by_field_name("arguments") else {
        return Some("");
    };
    let paths = children(args)
        .into_iter()
        .filter_map(|arg| {
            if arg.kind() == "string_literal" {
                return literal(text(arg, file));
            }
            if arg.kind() == "element_value_pair"
                && matches!(field(arg, "key", file), Some("path" | "value"))
            {
                let value = arg.child_by_field_name("value")?;
                return literal(text(value, file));
            }
            None
        })
        .collect::<Vec<_>>();
    let path_arg = children(args).iter().any(|arg| {
        arg.kind() != "element_value_pair"
            || matches!(field(*arg, "key", file), Some("path" | "value"))
    });
    match paths.as_slice() {
        [path] => Some(path),
        [] if !path_arg => Some(""),
        _ => None,
    }
}

fn table_mapping(class: &Class<'_>) -> Option<(String, String)> {
    let table = annotation(class.declaration, class.file, "Table")?;
    let arguments = table.child_by_field_name("arguments")?;
    let mut schema = "public".to_owned();
    let mut name = None;
    for pair in children(arguments) {
        if let Some(value) = field(pair, "value", class.file).and_then(literal) {
            match field(pair, "key", class.file) {
                Some("name") => name = Some(value.to_owned()),
                Some("schema") if !value.is_empty() => schema = value.to_owned(),
                _ => {}
            }
        }
    }
    Some((schema, name?))
}

fn migration_tables(files: &[SourceFile]) -> BTreeMap<(String, String), (&SourceFile, usize)> {
    let mut tables = BTreeMap::new();
    for file in files.iter().filter(|file| file.path.ends_with(".sql")) {
        let tokens = sql_tokens(&file.text);
        for (index, (token, line)) in tokens.iter().enumerate() {
            if !token.eq_ignore_ascii_case("create")
                || !tokens
                    .get(index + 1)
                    .is_some_and(|(token, _)| token.eq_ignore_ascii_case("table"))
            {
                continue;
            }
            let mut offset = index + 2;
            if tokens
                .get(offset)
                .is_some_and(|(token, _)| token.eq_ignore_ascii_case("if"))
            {
                offset += 3;
            }
            let Some((first, _)) = tokens.get(offset) else {
                continue;
            };
            let (schema, table) = if tokens
                .get(offset + 1)
                .is_some_and(|(token, _)| token == ".")
            {
                let Some((table, _)) = tokens.get(offset + 2) else {
                    continue;
                };
                (first.as_str(), table.as_str())
            } else {
                ("public", first.as_str())
            };
            tables
                .entry((schema.to_owned(), table.to_owned()))
                .or_insert((file, *line));
        }
    }
    tables
}

fn sql_tokens(source: &str) -> Vec<(String, usize)> {
    let mut cursor = source.chars().peekable();
    let mut line = 1;
    let mut result = Vec::new();
    while let Some(c) = cursor.next() {
        match c {
            '\n' => line += 1,
            '-' if cursor.peek() == Some(&'-') => {
                for next in cursor.by_ref() {
                    if next == '\n' {
                        line += 1;
                        break;
                    }
                }
            }
            '/' if cursor.peek() == Some(&'*') => {
                cursor.next();
                let mut previous = ' ';
                for next in cursor.by_ref() {
                    if next == '\n' {
                        line += 1;
                    }
                    if previous == '*' && next == '/' {
                        break;
                    }
                    previous = next;
                }
            }
            '\'' | '"' => {
                let mut value = String::new();
                let start = line;
                while let Some(next) = cursor.next() {
                    if next == '\n' {
                        line += 1;
                    }
                    if next == c {
                        if cursor.peek() != Some(&c) {
                            break;
                        }
                        cursor.next();
                    }
                    value.push(next);
                }
                if c == '"' {
                    result.push((value, start));
                }
            }
            '.' => result.push((".".to_owned(), line)),
            c if c.is_ascii_alphabetic() || c == '_' => {
                let mut name = String::from(c);
                while let Some(next) =
                    cursor.next_if(|next| next.is_ascii_alphanumeric() || *next == '_')
                {
                    name.push(next);
                }
                result.push((name.to_ascii_lowercase(), line));
            }
            _ => {}
        }
    }
    result
}
