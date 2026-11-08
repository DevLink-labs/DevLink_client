mod frontend;
mod java;

use super::git::SourceFile;
use anyhow::{Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use tree_sitter::{Node, Parser, Tree};

#[derive(Serialize)]
pub(super) struct Graph {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<Edge>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct GraphNode {
    id: String,
    kind: &'static str,
    label: String,
    source: Location,
    http_route: Option<Route>,
    schema_name: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Location {
    relative_path: String,
    line: usize,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Route {
    method: String,
    path_template: String,
}

#[derive(Serialize, Ord, PartialOrd, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) struct Edge {
    source_id: String,
    target_id: String,
    relation_type: &'static str,
}

#[derive(Default)]
struct Builder {
    nodes: BTreeMap<String, GraphNode>,
    edges: BTreeSet<Edge>,
}

impl Builder {
    fn node(
        &mut self,
        kind: &'static str,
        file: &SourceFile,
        line: usize,
        symbol: &str,
        label: &str,
    ) -> String {
        let id = format!(
            "{:x}",
            Sha256::digest(format!("{}\0{kind}\0{symbol}", file.path).as_bytes())
        );
        self.nodes.entry(id.clone()).or_insert_with(|| GraphNode {
            id: id.clone(),
            kind,
            label: label
                .chars()
                .filter(|c| !c.is_control())
                .take(600)
                .collect(),
            source: Location {
                relative_path: file.path.clone(),
                line,
            },
            http_route: None,
            schema_name: None,
        });
        id
    }

    fn route(&mut self, id: &str, route: Option<Route>) {
        self.nodes
            .get_mut(id)
            .expect("graph node exists")
            .http_route = route;
    }

    fn schema(&mut self, id: &str, name: String) {
        self.nodes
            .get_mut(id)
            .expect("graph node exists")
            .schema_name = Some(name);
    }

    fn edge(&mut self, source: &str, target: &str, relation_type: &'static str) {
        self.edges.insert(Edge {
            source_id: source.to_owned(),
            target_id: target.to_owned(),
            relation_type,
        });
    }

    fn import(&mut self, from: &SourceFile, to: &SourceFile) {
        if from.path == to.path {
            return;
        }
        let first = self.node("MODULE", from, 1, "", &from.path);
        let second = self.node("MODULE", to, 1, "", &to.path);
        self.edge(&first, &second, "IMPORT");
    }
}

struct Parsed<'a> {
    file: &'a SourceFile,
    tree: Tree,
}

pub(super) fn analyze(files: &[SourceFile], kind: &str) -> Result<Graph> {
    ensure!(
        matches!(kind, "FRONTEND" | "BACKEND"),
        "Unsupported dependency repository kind"
    );
    let mut parser = Parser::new();
    let mut parsed = Vec::new();
    for file in files {
        let suffix = file.path.rsplit_once('.').map(|(_, suffix)| suffix);
        let language = match (kind, suffix) {
            ("FRONTEND", Some("tsx")) => tree_sitter_typescript::LANGUAGE_TSX,
            ("FRONTEND", Some("ts")) => tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
            ("FRONTEND", Some("js" | "jsx")) => tree_sitter_javascript::LANGUAGE,
            ("BACKEND", Some("java")) => tree_sitter_java::LANGUAGE,
            _ => continue,
        };
        parser.set_language(&language.into())?;
        if let Some(tree) = parser
            .parse(&file.text, None)
            .filter(|tree| !tree.root_node().has_error())
        {
            parsed.push(Parsed { file, tree });
        }
    }
    ensure!(
        !parsed.is_empty(),
        "No supported source files could be parsed"
    );
    let mut graph = Builder::default();
    match kind {
        "FRONTEND" => frontend::collect(&parsed, &mut graph),
        _ => java::collect(&parsed, files, &mut graph),
    }
    ensure!(
        graph.nodes.len() <= 5000 && graph.edges.len() <= 10000,
        "Dependency graph exceeds supported size"
    );
    Ok(Graph {
        nodes: graph.nodes.into_values().collect(),
        edges: graph.edges.into_iter().collect(),
    })
}

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    node.named_children(&mut node.walk()).collect()
}

fn descendants(node: Node<'_>) -> Vec<Node<'_>> {
    let mut pending = vec![node];
    let mut result = Vec::new();
    while let Some(next) = pending.pop() {
        pending.extend(children(next).into_iter().rev());
        result.push(next);
    }
    result
}

fn text<'a>(node: Node<'_>, file: &'a SourceFile) -> &'a str {
    &file.text[node.byte_range()]
}

fn field<'a>(node: Node<'_>, name: &str, file: &'a SourceFile) -> Option<&'a str> {
    node.child_by_field_name(name).map(|node| text(node, file))
}

fn literal(value: &str) -> Option<&str> {
    let quote = value.as_bytes().first().copied()?;
    if !matches!(quote, b'\'' | b'"') || value.as_bytes().last() != Some(&quote) || value.len() < 2
    {
        return None;
    }
    let value = &value[1..value.len() - 1];
    (!value.contains('\\')).then_some(value)
}

fn route(method: &str, path: &str) -> Option<Route> {
    let method = method.to_ascii_uppercase();
    if !matches!(
        method.as_str(),
        "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS"
    ) {
        return None;
    }
    let path = path.split_once('?').map_or(path, |(path, _)| path);
    let path = path.strip_prefix('/')?.trim_end_matches('/');
    let valid = path.len() < 500
        && !path.is_empty()
        && path.split('/').all(|segment| {
            if let Some(parameter) = segment
                .strip_prefix('{')
                .and_then(|rest| rest.strip_suffix('}'))
            {
                return !parameter.is_empty()
                    && parameter
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_');
            }
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.~-".contains(&b))
        });
    valid.then(|| Route {
        method,
        path_template: format!("/{path}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(path: &str, text: &str) -> SourceFile {
        SourceFile {
            path: path.to_owned(),
            text: text.to_owned(),
        }
    }

    fn has_edge(graph: &Graph, from: &str, to: &str, relation: &str) -> bool {
        graph.edges.iter().any(|edge| {
            edge.relation_type == relation
                && graph
                    .nodes
                    .iter()
                    .any(|node| node.id == edge.source_id && node.label.starts_with(from))
                && graph
                    .nodes
                    .iter()
                    .any(|node| node.id == edge.target_id && node.label.starts_with(to))
        })
    }

    #[test]
    fn shared_component_links_both_pages_to_the_same_http_call_site() {
        let files = [
            source(
                "src/lib/apiClient.ts",
                r#"export const apiClient = axios.create({baseURL: `${API_BASE_URL}/api/v1`})"#,
            ),
            source(
                "src/pages/WorkspacePage.tsx",
                r#"import Conversation from "@/Conversation"; export default function WorkspacePage() { return <Conversation />; }"#,
            ),
            source(
                "src/pages/ArchivePage.jsx",
                r#"import Conversation from "../Conversation"; export default function ArchivePage() { return <Conversation />; }"#,
            ),
            source(
                "src/Conversation.tsx",
                r#"import { useHistory } from "./hooks"; export default function Conversation() { useHistory(3, 4); return <div />; }"#,
            ),
            source(
                "src/hooks.ts",
                r#"import { taskApi } from "./taskApi"; export function useHistory(teamId: number, taskId: number) { return useQuery({queryFn: () => taskApi.getHistory(teamId, taskId)}); }"#,
            ),
            source(
                "src/taskApi.ts",
                r#"
                import { apiClient } from "@/lib/apiClient";
                function path(teamId: number): string { return `/web/teams/${teamId}/tasks`; }
                export const taskApi = {
                    getHistory: (teamId: number, taskId: number) =>
                        apiClient.get<History>(`${path(teamId)}/${taskId}/events?${params}`, signal),
                };
            "#,
            ),
        ];
        let graph = analyze(&files, "FRONTEND").unwrap();
        assert!(has_edge(&graph, "WorkspacePage", "Conversation", "RENDERS"));
        assert!(has_edge(&graph, "ArchivePage", "Conversation", "RENDERS"));
        assert!(has_edge(&graph, "Conversation", "useHistory", "CALLS"));
        assert!(has_edge(
            &graph,
            "useHistory",
            "taskApi.getHistory",
            "CALLS"
        ));
        let routes = graph
            .nodes
            .iter()
            .filter_map(|node| node.http_route.as_ref())
            .collect::<Vec<_>>();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].method, "GET");
        assert_eq!(
            routes[0].path_template,
            "/api/v1/web/teams/{teamId}/tasks/{taskId}/events"
        );
    }

    #[test]
    fn spring_handler_reaches_the_table_through_a_service_helper() {
        let files = [
            source(
                "src/main/java/demo/TaskController.java",
                r#"
                package demo;
                @RestController
                @RequestMapping("/api/v1")
                public class TaskController {
                    private final TaskService service;
                    @GetMapping("/web/tasks/{id}/events")
                    Object history(Long id) { return service.history(id); }
                }
            "#,
            ),
            source(
                "src/main/java/demo/TaskService.java",
                r#"
                package demo;
                @Service
                public class TaskService {
                    private final TaskRepository repository;
                    Object history(Long id) { return read(id); }
                    private Object read(Long id) { return repository.findById(id); }
                }
            "#,
            ),
            source(
                "src/main/java/demo/TaskRepository.java",
                "package demo; public interface TaskRepository extends JpaRepository<Task, Long> {}",
            ),
            source(
                "src/main/java/demo/Task.java",
                r#"package demo; @Table(name = "tasks") public class Task {}"#,
            ),
            source(
                "src/main/resources/db/migration/V1__init.sql",
                "-- CREATE TABLE ignored\nCREATE TABLE public.tasks (id BIGINT);",
            ),
        ];
        let graph = analyze(&files, "BACKEND").unwrap();
        assert!(has_edge(
            &graph,
            "GET /api/v1/web/tasks/{id}/events",
            "TaskService.history",
            "CALLS"
        ));
        assert!(has_edge(
            &graph,
            "TaskService.history",
            "TaskRepository",
            "CALLS"
        ));
        assert!(has_edge(&graph, "TaskRepository", "tasks", "USES_TABLE"));
    }
}
