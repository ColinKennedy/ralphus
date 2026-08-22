//! ASCII and Graphviz DOT rendering for `ralphus graph`, ported from
//! `cli/src/ralphus/graphview.py`. The daemon returns only `{nodes, edges}`
//! -- all layout/rendering happens here, CLI-side.

use std::collections::{BTreeSet, HashMap};

use serde_json::Value;

/// Groups nodes into topological levels: level 0 has no prerequisites among
/// `node_ids`, level 1's prerequisites are all in level 0, etc. Deterministic
/// (nodes within a level are sorted). A residual cycle (shouldn't happen --
/// the daemon rejects cycles at submit time) dumps whatever's left as one
/// final level rather than looping forever.
fn levels(node_ids: &[String], edges: &[(String, String)]) -> Vec<Vec<String>> {
    let mut incoming: HashMap<String, BTreeSet<String>> = node_ids
        .iter()
        .map(|n| (n.clone(), BTreeSet::new()))
        .collect();
    for (from, to) in edges {
        if let Some(set) = incoming.get_mut(to) {
            set.insert(from.clone());
        }
    }
    let mut remaining = incoming;
    let mut placed: BTreeSet<String> = BTreeSet::new();
    let mut out: Vec<Vec<String>> = Vec::new();

    while !remaining.is_empty() {
        let mut ready: Vec<String> = remaining
            .iter()
            .filter(|(_, deps)| deps.is_subset(&placed))
            .map(|(n, _)| n.clone())
            .collect();
        ready.sort();
        if ready.is_empty() {
            ready = remaining.keys().cloned().collect();
            ready.sort();
        }
        for n in &ready {
            placed.insert(n.clone());
            remaining.remove(n);
        }
        out.push(ready);
    }
    out
}

fn labels_and_edges(
    nodes: &[Value],
    edges: &[Value],
) -> (HashMap<String, String>, Vec<(String, String)>) {
    let labels: HashMap<String, String> = nodes
        .iter()
        .filter_map(|n| {
            let id = n["id"].as_str()?.to_string();
            let label = n["label"].as_str().unwrap_or(&id).to_string();
            Some((id, label))
        })
        .collect();
    let edge_pairs: Vec<(String, String)> = edges
        .iter()
        .filter_map(|e| {
            Some((
                e["from"].as_str()?.to_string(),
                e["to"].as_str()?.to_string(),
            ))
        })
        .collect();
    (labels, edge_pairs)
}

/// Renders a layered, indented ASCII view. `nodes` must each have `id`; a
/// `label` key (if present) is shown alongside the id.
#[must_use]
pub fn render_ascii(nodes: &[Value], edges: &[Value]) -> String {
    let (labels, edge_pairs) = labels_and_edges(nodes, edges);
    let node_ids: Vec<String> = labels.keys().cloned().collect();

    let mut incoming: HashMap<String, Vec<String>> =
        node_ids.iter().map(|n| (n.clone(), Vec::new())).collect();
    for (from, to) in &edge_pairs {
        if let Some(list) = incoming.get_mut(to) {
            list.push(from.clone());
        }
    }

    let mut lines = Vec::new();
    for (i, level) in levels(&node_ids, &edge_pairs).iter().enumerate() {
        lines.push(format!("level {i}:"));
        for n in level {
            let label = &labels[n];
            let head = if label == n {
                format!("  {n}")
            } else {
                format!("  {n}  ({label})")
            };
            let deps = incoming.get(n).cloned().unwrap_or_default();
            let tail = if deps.is_empty() {
                String::new()
            } else {
                format!("  <- {}", deps.join(", "))
            };
            lines.push(format!("{head}{tail}"));
        }
    }
    lines.join("\n")
}

/// Renders `digraph { ... }` Graphviz source.
#[must_use]
pub fn render_dot(nodes: &[Value], edges: &[Value]) -> String {
    let (labels, _) = labels_and_edges(nodes, edges);
    let mut lines = vec!["digraph {".to_string()];
    // BTreeMap would sort by key; Python dict preserves insertion order, but
    // DOT node declaration order has no semantic meaning, so sorting for
    // deterministic test/CLI output is a strict improvement here.
    let mut ordered: Vec<(&String, &String)> = labels.iter().collect();
    ordered.sort_by_key(|(id, _)| (*id).clone());
    for (node_id, label) in ordered {
        let escaped = label.replace('\\', "\\\\").replace('"', "\\\"");
        lines.push(format!("  \"{node_id}\" [label=\"{escaped}\"];"));
    }
    for e in edges {
        let from = e["from"].as_str().unwrap_or_default();
        let to = e["to"].as_str().unwrap_or_default();
        lines.push(format!("  \"{from}\" -> \"{to}\";"));
    }
    lines.push("}".to_string());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn render_ascii_groups_into_levels_with_deps() {
        let nodes = vec![json!({"id": "a"}), json!({"id": "b"}), json!({"id": "c"})];
        let edges = vec![
            json!({"from": "a", "to": "b"}),
            json!({"from": "b", "to": "c"}),
        ];
        let out = render_ascii(&nodes, &edges);
        assert!(out.contains("level 0:"));
        assert!(out.contains("level 2:"));
        assert!(out.contains("<- b"));
    }

    #[test]
    fn render_ascii_uses_label_when_present() {
        let nodes = vec![json!({"id": "a", "label": "Task A"})];
        let out = render_ascii(&nodes, &[]);
        assert!(out.contains("a  (Task A)"));
    }

    #[test]
    fn render_dot_escapes_quotes_and_backslashes() {
        let nodes = vec![json!({"id": "a", "label": "say \"hi\"\\now"})];
        let out = render_dot(&nodes, &[]);
        assert!(out.contains(r#"label="say \"hi\"\\now""#));
    }

    #[test]
    fn render_dot_wraps_with_digraph_block() {
        let nodes = vec![json!({"id": "a"})];
        let edges = vec![json!({"from": "a", "to": "b"})];
        let out = render_dot(&nodes, &edges);
        assert!(out.starts_with("digraph {"));
        assert!(out.ends_with('}'));
        assert!(out.contains("\"a\" -> \"b\";"));
    }

    #[test]
    fn levels_dumps_residual_cycle_as_one_level_instead_of_looping() {
        let ids = vec!["a".to_string(), "b".to_string()];
        let edges = vec![
            ("a".to_string(), "b".to_string()),
            ("b".to_string(), "a".to_string()),
        ];
        let result = levels(&ids, &edges);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], vec!["a".to_string(), "b".to_string()]);
    }
}
