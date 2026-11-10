use super::{Builder, Parsed, SourceFile, children, descendants, field, literal, route, text};
use std::collections::BTreeMap;
use tree_sitter::Node;

struct Function<'a> {
    file: &'a SourceFile,
    callable: Node<'a>,
    name: String,
    id: String,
}

type Imports = BTreeMap<String, (String, String)>;

pub(super) fn collect(parsed: &[Parsed<'_>], graph: &mut Builder) {
    let files: BTreeMap<_, _> = parsed
        .iter()
        .map(|entry| (entry.file.path.as_str(), entry.file))
        .collect();
    let mut functions = Vec::new();
    let mut imports = BTreeMap::new();
    for entry in parsed {
        let file = entry.file;
        let mut bindings = Imports::new();
        for node in descendants(entry.tree.root_node()) {
            if matches!(node.kind(), "import_statement" | "export_statement")
                && let Some(target) = field(node, "source", file)
                    .and_then(literal)
                    .and_then(|specifier| resolve_path(&files, &file.path, specifier))
            {
                graph.import(file, target);
                import_symbols(node, file, target, &mut bindings);
            }
            match node.kind() {
                "function_declaration" => {
                    if let Some(name) = field(node, "name", file) {
                        add_function(file, node, name.to_owned(), &mut functions, graph);
                    }
                }
                "variable_declarator" => {
                    let (Some(name), Some(value)) =
                        (field(node, "name", file), node.child_by_field_name("value"))
                    else {
                        continue;
                    };
                    if callable(value) {
                        add_function(file, value, name.to_owned(), &mut functions, graph);
                    } else if value.kind() == "object" {
                        for pair in children(value) {
                            let (Some(key), Some(function)) =
                                (field(pair, "key", file), pair.child_by_field_name("value"))
                            else {
                                continue;
                            };
                            if callable(function) {
                                let key = literal(key).unwrap_or(key);
                                add_function(
                                    file,
                                    function,
                                    format!("{name}.{key}"),
                                    &mut functions,
                                    graph,
                                );
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        imports.insert(file.path.as_str(), bindings);
    }
    let index = functions
        .iter()
        .map(|function| {
            (
                (function.file.path.as_str(), function.name.as_str()),
                function,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let prefix = api_prefix(parsed, &functions);
    for function in &functions {
        let Some(body) = function.callable.child_by_field_name("body") else {
            continue;
        };
        let bindings = &imports[function.file.path.as_str()];
        let parameters = parameters(function)
            .into_iter()
            .map(|name| (name.to_owned(), format!("{{{name}}}")))
            .collect::<BTreeMap<_, _>>();
        for node in body_nodes(body) {
            if matches!(
                node.kind(),
                "jsx_opening_element" | "jsx_self_closing_element"
            ) {
                if let Some(target) = field(node, "name", function.file)
                    .filter(|name| name.starts_with(|c: char| c.is_ascii_uppercase()))
                    .and_then(|name| resolve_symbol(function, name, bindings, &index))
                {
                    graph.edge(&function.id, &target.id, "RENDERS");
                }
            } else if node.kind() == "call_expression" {
                let Some(called) = field(node, "function", function.file) else {
                    continue;
                };
                if let Some(target) = resolve_symbol(function, called, bindings, &index) {
                    graph.edge(&function.id, &target.id, "CALLS");
                }
                let Some((receiver, method)) = called.split_once('.') else {
                    continue;
                };
                if !bindings
                    .get(receiver)
                    .is_some_and(|(path, export)| export == "apiClient" && is_api_client(path))
                    || !matches!(
                        method,
                        "get" | "post" | "put" | "patch" | "delete" | "head" | "options"
                    )
                {
                    continue;
                }
                let target = node
                    .child_by_field_name("arguments")
                    .and_then(|arguments| arguments.named_child(0))
                    .and_then(|value| expand(function.file, value, &parameters, &functions, 0));
                let http_route = target.and_then(|target| {
                    if !target.starts_with('/') || target.starts_with("//") {
                        return None;
                    }
                    route(method, &format!("{prefix}{target}"))
                });
                let label = http_route.as_ref().map_or_else(
                    || format!("{} · {}", function.name, method.to_ascii_uppercase()),
                    |route| {
                        format!(
                            "{} · {} {}",
                            function.name, route.method, route.path_template
                        )
                    },
                );
                let symbol = format!(
                    "{}:{}",
                    function.name,
                    node.start_byte() - function.callable.start_byte()
                );
                let id = graph.node(
                    "HTTP_CLIENT",
                    function.file,
                    node.start_position().row + 1,
                    &symbol,
                    &label,
                );
                graph.route(&id, http_route);
                graph.edge(&function.id, &id, "CALLS");
            }
        }
    }
}

fn callable(node: Node<'_>) -> bool {
    matches!(node.kind(), "arrow_function" | "function_expression")
}

fn add_function<'a>(
    file: &'a SourceFile,
    callable: Node<'a>,
    name: String,
    functions: &mut Vec<Function<'a>>,
    graph: &mut Builder,
) {
    if name.is_empty() {
        return;
    }
    let upper = name.starts_with(|c: char| c.is_ascii_uppercase());
    let kind = if upper && name.ends_with("Page") {
        "PAGE"
    } else if upper {
        "COMPONENT"
    } else if name
        .strip_prefix("use")
        .is_some_and(|name| name.starts_with(|c: char| c.is_ascii_uppercase()))
    {
        "HOOK"
    } else {
        "MODULE"
    };
    let id = graph.node(kind, file, callable.start_position().row + 1, &name, &name);
    functions.push(Function {
        file,
        callable,
        name,
        id,
    });
}

fn body_nodes(body: Node<'_>) -> Vec<Node<'_>> {
    let mut stack = vec![body];
    let mut output = Vec::new();
    while let Some(node) = stack.pop() {
        if node.kind() == "function_declaration" {
            continue;
        }
        stack.extend(children(node).into_iter().rev());
        output.push(node);
    }
    output
}

fn resolve_path<'a>(
    files: &BTreeMap<&str, &'a SourceFile>,
    from: &str,
    specifier: &str,
) -> Option<&'a SourceFile> {
    let raw = if let Some(path) = specifier.strip_prefix("@/") {
        format!("src/{path}")
    } else if specifier.starts_with("./") || specifier.starts_with("../") {
        let parent = from.rsplit_once('/').map_or("", |(parent, _)| parent);
        format!("{parent}/{specifier}")
    } else {
        return None;
    };
    let mut parts = Vec::new();
    for part in raw.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            _ => parts.push(part),
        }
    }
    let path = parts.join("/");
    if let Some(file) = files.get(path.as_str()) {
        return Some(*file);
    }
    let base = path
        .strip_suffix(".js")
        .or_else(|| path.strip_suffix(".jsx"))
        .unwrap_or(&path);
    for candidate in [base.to_owned(), format!("{base}/index")] {
        for extension in ["ts", "tsx", "js", "jsx"] {
            if let Some(file) = files.get(format!("{candidate}.{extension}").as_str()) {
                return Some(*file);
            }
        }
    }
    None
}

fn import_symbols(node: Node<'_>, file: &SourceFile, target: &SourceFile, result: &mut Imports) {
    let Some(clause) = children(node)
        .into_iter()
        .find(|item| item.kind() == "import_clause")
    else {
        return;
    };
    for item in children(clause) {
        if item.kind() == "identifier" {
            result.insert(
                text(item, file).to_owned(),
                (target.path.clone(), "default".to_owned()),
            );
        } else if item.kind() == "named_imports" {
            for specifier in children(item) {
                if let Some(name) = field(specifier, "name", file) {
                    let alias = field(specifier, "alias", file).unwrap_or(name);
                    result.insert(alias.to_owned(), (target.path.clone(), name.to_owned()));
                }
            }
        }
    }
}

fn resolve_symbol<'a>(
    function: &Function<'_>,
    name: &str,
    bindings: &Imports,
    index: &BTreeMap<(&str, &str), &'a Function<'a>>,
) -> Option<&'a Function<'a>> {
    let (prefix, suffix) = name
        .split_once('.')
        .map_or((name, None), |(base, tail)| (base, Some(tail)));
    let Some((path, export)) = bindings.get(prefix) else {
        return index.get(&(function.file.path.as_str(), name)).copied();
    };
    let export = if export == "default" {
        path.rsplit('/').next()?.split('.').next()?
    } else {
        export
    };
    let symbol = suffix.map_or_else(|| export.to_owned(), |suffix| format!("{export}.{suffix}"));
    index.get(&(path.as_str(), symbol.as_str())).copied()
}

fn parameters<'a>(function: &Function<'a>) -> Vec<&'a str> {
    let Some(parameters) = function
        .callable
        .child_by_field_name("parameters")
        .or_else(|| function.callable.child_by_field_name("parameter"))
    else {
        return Vec::new();
    };
    if parameters.kind() == "identifier" {
        return vec![text(parameters, function.file)];
    }
    children(parameters)
        .into_iter()
        .filter_map(|node| {
            let pattern = node.child_by_field_name("pattern").unwrap_or(node);
            (pattern.kind() == "identifier").then(|| text(pattern, function.file))
        })
        .collect()
}

fn is_api_client(path: &str) -> bool {
    path.rsplit('/')
        .next()
        .is_some_and(|name| name.starts_with("apiClient."))
}

fn api_prefix(parsed: &[Parsed<'_>], functions: &[Function<'_>]) -> String {
    parsed
        .iter()
        .filter(|item| is_api_client(&item.file.path))
        .find_map(|item| {
            descendants(item.tree.root_node())
                .into_iter()
                .find_map(|node| {
                    if node.kind() != "pair" || field(node, "key", item.file) != Some("baseURL") {
                        return None;
                    }
                    let origin = BTreeMap::from([("API_BASE_URL".to_owned(), String::new())]);
                    let raw = expand(
                        item.file,
                        node.child_by_field_name("value")?,
                        &origin,
                        functions,
                        0,
                    )?;
                    if raw.starts_with('/') {
                        return Some(raw.trim_end_matches('/').to_owned());
                    }
                    url::Url::parse(&raw)
                        .ok()
                        .map(|url| url.path().trim_end_matches('/').to_owned())
                })
        })
        .unwrap_or_default()
}

fn expand(
    file: &SourceFile,
    node: Node<'_>,
    values: &BTreeMap<String, String>,
    functions: &[Function<'_>],
    depth: usize,
) -> Option<String> {
    if depth > 6 {
        return None;
    }
    match node.kind() {
        "string" => literal(text(node, file)).map(str::to_owned),
        "identifier" => values.get(text(node, file)).cloned(),
        "template_string" => {
            let mut result = String::new();
            for part in children(node) {
                match part.kind() {
                    "string_fragment" => result.push_str(text(part, file)),
                    "template_substitution" => {
                        result.push_str(&expand(
                            file,
                            part.named_child(0)?,
                            values,
                            functions,
                            depth + 1,
                        )?);
                    }
                    _ => return None,
                }
                if let Some(query) = result.find('?') {
                    result.truncate(query);
                    break;
                }
            }
            Some(result)
        }
        "call_expression" => {
            let name = field(node, "function", file)?;
            let mut candidates = functions
                .iter()
                .filter(|function| function.file.path == file.path && function.name == name);
            let helper = candidates.next()?;
            if candidates.next().is_some() {
                return None;
            }
            let args = children(node.child_by_field_name("arguments")?);
            let names = parameters(helper);
            if args.len() != names.len() {
                return None;
            }
            let mut bound = BTreeMap::new();
            for (name, argument) in names.into_iter().zip(args) {
                bound.insert(
                    name.to_owned(),
                    expand(file, argument, values, functions, depth + 1)?,
                );
            }
            let body = helper.callable.child_by_field_name("body")?;
            let result = if body.kind() == "statement_block" {
                let statements = children(body)
                    .into_iter()
                    .filter(|node| node.kind() != "comment")
                    .collect::<Vec<_>>();
                if statements.len() != 1 || statements[0].kind() != "return_statement" {
                    return None;
                }
                statements[0].named_child(0)?
            } else {
                body
            };
            expand(file, result, &bound, functions, depth + 1)
        }
        _ => None,
    }
}
