//! Phase 3 cross-table validation.
//!
//! Walks every `ref()` call in the AST and answers four questions:
//!
//! 1. Does the target table exist? → `UndeclaredRefTable`
//! 2. Does the target column exist in that table? → `UndeclaredRefColumn`
//! 3. Is the table referencing itself? → `SelfReference` (Phase 4 may relax this)
//! 4. Do the directed ref-edges form a cycle? → `CyclicReference` with every
//!    edge in the cycle named for clarity.
//!
//! It also enforces table/generate parity:
//!
//! * every declared `table` has exactly one `generate`
//! * every `generate NAME: N` references a declared table
//!
//! Output: an ordered list of table names (post-order DFS over the ref graph,
//! alphabetically tie-broken) and the set of (table, column) pairs that are
//! ref targets — the materialisation plan the engine and pool need.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::ast::{Call, File, Value};
use crate::error::{CycleEdge, SemanticError};

#[derive(Debug, Default)]
pub(super) struct RelationsReport {
    pub errors: Vec<SemanticError>,
    pub topo_order: Vec<String>,
    pub referenced: BTreeMap<String, BTreeSet<String>>,
}

pub(super) fn analyze(file: &File) -> RelationsReport {
    let mut report = RelationsReport::default();

    // --- Table/generate parity ------------------------------------------
    let declared: BTreeMap<&str, (usize, usize)> = file
        .tables
        .iter()
        .map(|t| (t.name.as_str(), (t.line, t.col)))
        .collect();
    let generated: BTreeSet<&str> = file.generate.iter().map(|g| g.table.as_str()).collect();

    for t in &file.tables {
        if !generated.contains(t.name.as_str()) {
            report.errors.push(SemanticError::MissingGenerate {
                table: t.name.clone(),
                table_line: t.line,
                table_col: t.col,
            });
        }
    }
    for g in &file.generate {
        if !declared.contains_key(g.table.as_str()) {
            report.errors.push(SemanticError::GenerateForUnknownTable {
                line: g.line,
                col: g.col,
                name: g.table.clone(),
            });
        }
    }

    // --- Collect refs from every field ----------------------------------
    // Each entry is one `ref()` call: where it appears, and what it points at.
    // Filtering happens here so per-table iteration is local.
    let mut edges: Vec<CycleEdge> = Vec::new();
    for table in &file.tables {
        let table_name = table.name.as_str();
        for field in &table.fields {
            if field.call.function != "ref" {
                continue;
            }
            // Catalog check already rejected ill-shaped ref() calls. Skip
            // anything that doesn't look like the canonical form rather than
            // double-reporting.
            let target = match extract_target(&field.call) {
                Some(t) => t,
                None => continue,
            };

            // 1) target table must exist
            let target_decl = declared.get(target.0.as_str());
            if target_decl.is_none() {
                report.errors.push(SemanticError::UndeclaredRefTable {
                    line: field.call.line,
                    col: field.call.col,
                    table: target.0.clone(),
                });
                continue;
            }

            // 2) target column must exist in that table
            let target_table = file
                .tables
                .iter()
                .find(|t| t.name == target.0)
                .expect("declared but missing from tables?");
            let column_exists = target_table.fields.iter().any(|f| f.name == target.1);
            if !column_exists {
                report.errors.push(SemanticError::UndeclaredRefColumn {
                    line: field.call.line,
                    col: field.call.col,
                    table: target.0.clone(),
                    column: target.1.clone(),
                });
                continue;
            }

            // 3) no self-references
            if table_name == target.0 {
                report.errors.push(SemanticError::SelfReference {
                    line: field.call.line,
                    col: field.call.col,
                    table: target.0.clone(),
                    column: target.1.clone(),
                });
                continue;
            }

            edges.push(CycleEdge {
                from_table: table_name.to_string(),
                from_field: field.name.clone(),
                to_table: target.0,
                to_column: target.1,
                line: field.call.line,
                col: field.call.col,
            });
        }
    }

    // Build the materialisation plan from valid edges only — that way a typo
    // in a column name doesn't poison the pool plan with phantom keys.
    for e in &edges {
        report
            .referenced
            .entry(e.to_table.clone())
            .or_default()
            .insert(e.to_column.clone());
    }

    // --- Topological sort & cycle detection ------------------------------
    if let Some(order) = topo_sort(file, &edges, &mut report.errors) {
        report.topo_order = order;
    }

    report
}

/// Return `Some(target.0, target.1)` iff the call shape matches the canonical
/// `ref(T.C)` form. Catalog validation has already emitted errors for other
/// shapes — we just skip them here.
fn extract_target(call: &Call) -> Option<(String, String)> {
    if call.positional.len() != 1 || !call.kwargs.is_empty() {
        return None;
    }
    match &call.positional[0] {
        Value::ColumnRef { table, column } => Some((table.clone(), column.clone())),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeState {
    Unvisited,
    InProgress,
    Done,
}

/// DFS-based topological sort with cycle detection. Returns `Some(order)`
/// if the graph is acyclic and appends `CyclicReference` to `errors`
/// otherwise. Ties broken alphabetically so the order is stable.
fn topo_sort(
    file: &File,
    edges: &[CycleEdge],
    errors: &mut Vec<SemanticError>,
) -> Option<Vec<String>> {
    // Adjacency: from_table -> outgoing edges. BTreeMap keeps alphabetical
    // ordering implicit; the per-vertex sort below covers the within-list case.
    let mut adj: BTreeMap<String, Vec<CycleEdge>> = BTreeMap::new();
    for e in edges {
        adj.entry(e.from_table.clone()).or_default().push(e.clone());
    }

    let mut state: HashMap<String, NodeState> = file
        .tables
        .iter()
        .map(|t| (t.name.clone(), NodeState::Unvisited))
        .collect();
    if state.is_empty() {
        return Some(Vec::new());
    }

    // Alphabetical roots so the same graph always produces the same order.
    let mut roots: Vec<String> = state.keys().cloned().collect();
    roots.sort();

    let mut order: Vec<String> = Vec::new();
    let mut path: Vec<String> = Vec::new();
    let mut taken: Vec<CycleEdge> = Vec::new();

    let mut hit_cycle = false;
    for start in &roots {
        if hit_cycle {
            break;
        }
        if state[start] == NodeState::Unvisited {
            if let Err(cycle) = dfs(start, &adj, &mut state, &mut path, &mut taken, &mut order) {
                errors.push(SemanticError::CyclicReference { edges: cycle });
                hit_cycle = true;
            }
        }
    }
    if hit_cycle {
        return None;
    }
    Some(order)
}

fn dfs(
    node: &str,
    adj: &BTreeMap<String, Vec<CycleEdge>>,
    state: &mut HashMap<String, NodeState>,
    path: &mut Vec<String>,
    taken: &mut Vec<CycleEdge>,
    order: &mut Vec<String>,
) -> Result<(), Vec<CycleEdge>> {
    state.insert(node.to_string(), NodeState::InProgress);
    path.push(node.to_string());

    // Sorted, deduped outgoing edges so traversal is deterministic.
    let mut outs: Vec<&CycleEdge> = adj.get(node).map(|v| v.iter().collect()).unwrap_or_default();
    outs.sort_by(|a, b| {
        (a.to_table.as_str(), a.from_field.as_str())
            .cmp(&(b.to_table.as_str(), b.from_field.as_str()))
    });

    let mut seen_targets: BTreeSet<&str> = BTreeSet::new();
    for edge in outs {
        if !seen_targets.insert(edge.to_table.as_str()) {
            continue;
        }
        match state.get(edge.to_table.as_str()).copied() {
            Some(NodeState::Done) | None => continue,
            Some(NodeState::InProgress) => {
                // Cycle: walk back through `path` to the target.
                let idx = path
                    .iter()
                    .position(|n| n == &edge.to_table)
                    .expect("InProgress target must be on the path");
                let mut cycle = taken[idx..].to_vec();
                cycle.push(edge.clone());
                return Err(cycle);
            }
            Some(NodeState::Unvisited) => {
                taken.push(edge.clone());
                dfs(&edge.to_table, adj, state, path, taken, order)?;
                taken.pop();
            }
        }
    }

    state.insert(node.to_string(), NodeState::Done);
    path.pop();
    order.push(node.to_string());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;
    use crate::semantic::check;

    fn parse_ok(src: &str) -> File {
        parse(src).expect(&format!("parse failed for source"))
    }

    #[test]
    fn single_table_no_refs_is_fine() {
        let file = parse_ok(
            "output: sql\ntable t { id: sequence }\ngenerate t: 1\n",
        );
        let report = check(&file);
        assert!(report.is_ok(), "errors: {:?}", report.errors);
        assert_eq!(report.topo_order, vec!["t".to_string()]);
    }

    #[test]
    fn two_table_ref_resolves() {
        let src = r#"
            output: sql
            table users {
              id: sequence
              name: randomName()
            }
            table orders {
              id: sequence
              user_id: ref(users.id)
            }
            generate users: 10
            generate orders: 50
        "#;
        let file = parse_ok(src);
        let report = check(&file);
        assert!(report.is_ok(), "errors: {:?}", report.errors);
        assert_eq!(report.topo_order, vec!["users", "orders"]);
        assert!(report.referenced["users"].contains("id"));
    }

    #[test]
    fn undeclared_ref_table() {
        let src = r#"
            output: sql
            table orders {
              id: sequence
              user_id: ref(users.id)
            }
            generate orders: 1
        "#;
        let file = parse_ok(src);
        let report = check(&file);
        assert!(report
            .errors
            .iter()
            .any(|e| matches!(e, SemanticError::UndeclaredRefTable { table, .. } if table == "users")));
    }

    #[test]
    fn undeclared_ref_column() {
        let src = r#"
            output: sql
            table users { id: sequence }
            table orders {
              id: sequence
              who: ref(users.name)
            }
            generate users: 1
            generate orders: 1
        "#;
        let file = parse_ok(src);
        let report = check(&file);
        assert!(report.errors.iter().any(|e| matches!(
            e,
            SemanticError::UndeclaredRefColumn { table, column, .. }
                if table == "users" && column == "name"
        )));
    }

    #[test]
    fn self_reference_rejected() {
        let src = r#"
            output: sql
            table users {
              id: sequence
              parent_id: ref(users.id)
            }
            generate users: 1
        "#;
        let file = parse_ok(src);
        let report = check(&file);
        assert!(report.errors.iter().any(|e| matches!(
            e,
            SemanticError::SelfReference { table, column, .. }
                if table == "users" && column == "id"
        )));
    }

    #[test]
    fn missing_generate_for_declared_table() {
        let src = r#"
            output: sql
            table users { id: sequence }
            table orders { id: sequence }
            generate users: 1
        "#;
        let file = parse_ok(src);
        let report = check(&file);
        assert!(report.errors.iter().any(|e| matches!(
            e,
            SemanticError::MissingGenerate { table, .. } if table == "orders"
        )));
    }

    #[test]
    fn generate_for_unknown_table() {
        let src = r#"
            output: sql
            table users { id: sequence }
            generate users: 1
            generate ghosts: 1
        "#;
        let file = parse_ok(src);
        let report = check(&file);
        assert!(report.errors.iter().any(|e| matches!(
            e,
            SemanticError::GenerateForUnknownTable { name, .. } if name == "ghosts"
        )));
    }

    #[test]
    fn two_table_cycle_names_both_edges() {
        let src = r#"
            output: sql
            table users {
              id: sequence
              favourite_order: ref(orders.id)
            }
            table orders {
              id: sequence
              user_id: ref(users.id)
            }
            generate users: 1
            generate orders: 1
        "#;
        let file = parse_ok(src);
        let report = check(&file);
        let cycle = report.errors.iter().find_map(|e| match e {
            SemanticError::CyclicReference { edges } => Some(edges),
            _ => None,
        });
        let cycle = cycle.expect("expected CyclicReference, got: {report.errors:?}");
        assert_eq!(cycle.len(), 2, "two-table cycle has two edges");
        let tables: BTreeSet<&str> = cycle.iter().map(|e| e.from_table.as_str()).collect();
        assert!(tables.contains("users"));
        assert!(tables.contains("orders"));
    }

    #[test]
    fn three_table_cycle_names_all_edges() {
        let src = r#"
            output: sql
            table a { id: sequence  b_id: ref(b.id) }
            table b { id: sequence  c_id: ref(c.id) }
            table c { id: sequence  a_id: ref(a.id) }
            generate a: 1
            generate b: 1
            generate c: 1
        "#;
        let file = parse_ok(src);
        let report = check(&file);
        let cycle = report.errors.iter().find_map(|e| match e {
            SemanticError::CyclicReference { edges } => Some(edges),
            _ => None,
        });
        let cycle = cycle.expect("expected CyclicReference");
        assert_eq!(cycle.len(), 3);
    }

    #[test]
    fn topo_order_is_stable_for_disconnected_graphs() {
        // No refs — generation order falls back to alphabetical (the
        // tie-breaker).
        let src = r#"
            output: sql
            table zulu  { id: sequence }
            table alpha { id: sequence }
            table mike  { id: sequence }
            generate zulu: 1
            generate alpha: 1
            generate mike: 1
        "#;
        let file = parse_ok(src);
        let report = check(&file);
        assert!(report.is_ok());
        assert_eq!(report.topo_order, vec!["alpha", "mike", "zulu"]);
    }

    #[test]
    fn cycle_through_three_tables_with_extra_independent_table() {
        // Mixing a cyclic component with an independent acyclic node should
        // still report the cycle.
        let src = r#"
            output: sql
            table a { id: sequence  b_id: ref(b.id) }
            table b { id: sequence  a_id: ref(a.id) }
            table z { id: sequence }
            generate a: 1
            generate b: 1
            generate z: 1
        "#;
        let file = parse_ok(src);
        let report = check(&file);
        assert!(matches!(
            report.errors.iter().find(|e| matches!(e, SemanticError::CyclicReference { .. })),
            Some(_)
        ));
    }
}
