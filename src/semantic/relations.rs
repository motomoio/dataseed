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
use crate::error::{CycleEdge, PerParentSite, SemanticError};

#[derive(Debug, Default)]
pub(super) struct RelationsReport {
    pub errors: Vec<SemanticError>,
    pub topo_order: Vec<String>,
    pub referenced: BTreeMap<String, BTreeSet<String>>,
    /// child table → (parent table, parent column, (lo, hi))
    pub per_parent_owners: BTreeMap<String, (String, String, (u64, u64))>,
}

/// One resolved per_parent ref-site inside the child table being scanned.
/// Used only by the owner pass to decide whether a child has zero, one, or
/// many owning parents, and to build the diagnostic if it has many.
///
/// The owner pass does NOT re-check target existence — the edge pass below
/// already runs `UndeclaredRefTable`/`UndeclaredRefColumn` against every
/// `ref()` call (including those with kwargs), so a typo in `T` or `C` is
/// already reported there. The owner pass just records who owns whom.
#[derive(Debug)]
struct PerParentCandidate {
    parent_table: String,
    parent_column: String,
    range: (u64, u64),
    field_name: String,
    line: usize,
    col: usize,
}

pub(super) fn analyze(file: &File) -> RelationsReport {
    let mut report = RelationsReport::default();

    let declared: BTreeMap<&str, (usize, usize)> = file
        .tables
        .iter()
        .map(|t| (t.name.as_str(), (t.line, t.col)))
        .collect();
    let generated: BTreeSet<&str> = file.generate.iter().map(|g| g.table.as_str()).collect();

    // --- per_parent owner pass (runs BEFORE the MissingGenerate check
    // because owned children don't need an explicit `generate`).
    //
    // For each table we collect every per_parent ref-site. We do NOT check
    // target existence here — the edge pass below catches that for every
    // ref() call (kwargs or not) and emits UndeclaredRefTable/Column. The
    // owner pass just records ownership: zero sites → not owned; one site
    // → record the owner and (if needed) reject any conflicting explicit
    // `generate`; two or more → emit MultiplePerParentOwners and skip
    // recording an owner.
    for table in &file.tables {
        let mut owners_in_this_table: Vec<PerParentCandidate> = Vec::new();

        for field in &table.fields {
            if field.call.function != "ref" {
                continue;
            }
            let Some(range) = per_parent_of(&field.call) else {
                continue;
            };
            let Some((parent_table, parent_column)) = extract_target(&field.call) else {
                continue;
            };

            owners_in_this_table.push(PerParentCandidate {
                parent_table,
                parent_column,
                range,
                field_name: field.name.clone(),
                line: field.call.line,
                col: field.call.col,
            });
        }

        match owners_in_this_table.len() {
            0 => {}
            1 => {
                let only = owners_in_this_table.into_iter().next().unwrap();
                if let Some(g) = file.generate.iter().find(|g| g.table == table.name) {
                    report
                        .errors
                        .push(SemanticError::ExplicitGenerateConflictsWithPerParent {
                            child: table.name.clone(),
                            parent: only.parent_table.clone(),
                            field: only.field_name.clone(),
                            generate_line: g.line,
                            generate_col: g.col,
                        });
                }
                report.per_parent_owners.insert(
                    table.name.clone(),
                    (only.parent_table, only.parent_column, only.range),
                );
            }
            _ => {
                let first = &owners_in_this_table[0];
                let second = &owners_in_this_table[1];
                report.errors.push(SemanticError::MultiplePerParentOwners {
                    child: table.name.clone(),
                    sites: Box::new((
                        PerParentSite {
                            parent: first.parent_table.clone(),
                            field: first.field_name.clone(),
                            line: first.line,
                            col: first.col,
                        },
                        PerParentSite {
                            parent: second.parent_table.clone(),
                            field: second.field_name.clone(),
                            line: second.line,
                            col: second.col,
                        },
                    )),
                });
            }
        }
    }

    // --- Table/generate parity ------------------------------------------
    for t in &file.tables {
        // per_parent-owned children derive their row count from the parent
        // pool; they intentionally have no `generate` directive.
        if report.per_parent_owners.contains_key(&t.name) {
            continue;
        }
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

/// Return `Some((table, column))` iff the call has exactly one positional
/// arg that is a `T.C` column reference. We deliberately tolerate kwargs
/// here so the canonical `ref(T.C, per_parent: lo..hi)` form flows through
/// the same edge-collection logic as plain `ref(T.C)` — that's how
/// per_parent refs get target-existence diagnostics and contribute to the
/// topo order and materialisation plan. Catalog validation has already
/// emitted errors for other shapes; we just skip them here.
fn extract_target(call: &Call) -> Option<(String, String)> {
    if call.positional.len() != 1 {
        return None;
    }
    match &call.positional[0] {
        Value::ColumnRef { table, column } => Some((table.clone(), column.clone())),
        _ => None,
    }
}

/// If `call` carries a `per_parent: lo..hi` kwarg with non-negative bounds,
/// return `Some((lo, hi))`. Anything else (no kwarg, wrong type, negatives)
/// returns `None` — the catalog pass already errored on shape problems.
fn per_parent_of(call: &Call) -> Option<(u64, u64)> {
    for (k, v) in &call.kwargs {
        if k == "per_parent" {
            if let Value::Range { lo, hi } = v {
                if *lo >= 0 && *hi >= 0 {
                    return Some((*lo as u64, *hi as u64));
                }
            }
        }
    }
    None
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
    fn per_parent_forbids_explicit_generate_for_child() {
        let src = r#"
            output: sql
            table users { id: sequence }
            table posts {
              id: sequence
              author_id: ref(users.id, per_parent: 0..10)
            }
            generate users: 100
            generate posts: 50
        "#;
        let file = parse_ok(src);
        let report = check(&file);
        assert!(report.errors.iter().any(|e| matches!(
            e,
            SemanticError::ExplicitGenerateConflictsWithPerParent { child, .. } if child == "posts"
        )));
    }

    #[test]
    fn per_parent_two_owners_rejected() {
        let src = r#"
            output: sql
            table a { id: sequence }
            table b { id: sequence }
            table xs {
              id:   sequence
              a_id: ref(a.id, per_parent: 1..3)
              b_id: ref(b.id, per_parent: 1..3)
            }
            generate a: 10
            generate b: 10
        "#;
        let file = parse_ok(src);
        let report = check(&file);
        assert!(report.errors.iter().any(|e| matches!(
            e,
            SemanticError::MultiplePerParentOwners { child, .. } if child == "xs"
        )));
    }

    #[test]
    fn per_parent_undeclared_parent_table_reported() {
        // Regression for the kwarg-skip bug: before this fix, a per_parent
        // ref slipped past extract_target() (which rejected any kwargs) and
        // never reached the existence check, so a bad parent name produced
        // no diagnostic at semantic-check time and panicked later at runtime.
        // Note: a bare `generate posts: 1` is needed so the parser accepts
        // the file. The semantic check is what we're exercising — it should
        // report the undeclared parent regardless of whether the child
        // ends up owned or not.
        let src = r#"
            output: sql
            table posts {
              id: sequence
              author_id: ref(ghosts.id, per_parent: 1..3)
            }
            generate posts: 1
        "#;
        let file = parse_ok(src);
        let report = check(&file);
        assert!(
            report.errors.iter().any(|e| matches!(
                e,
                SemanticError::UndeclaredRefTable { table, .. } if table == "ghosts"
            )),
            "expected UndeclaredRefTable(ghosts); got: {:?}",
            report.errors,
        );
    }

    #[test]
    fn per_parent_contributes_topo_edge() {
        // Regression: the topo pass used to skip per_parent refs entirely,
        // so children could come before their parents in the generation
        // order and `referenced[parent].column` was never populated — the
        // pool was empty when the child tried to draw from it.
        let src = r#"
            output: sql
            table users { id: sequence }
            table posts {
              id: sequence
              author_id: ref(users.id, per_parent: 1..3)
            }
            generate users: 5
        "#;
        let file = parse_ok(src);
        let report = check(&file);
        assert!(report.is_ok(), "errors: {:?}", report.errors);
        let users_idx = report
            .topo_order
            .iter()
            .position(|t| t == "users")
            .expect("users in topo order");
        let posts_idx = report
            .topo_order
            .iter()
            .position(|t| t == "posts")
            .expect("posts in topo order");
        assert!(
            users_idx < posts_idx,
            "users must precede posts; got {:?}",
            report.topo_order
        );
        assert!(
            report
                .referenced
                .get("users")
                .map_or(false, |cols| cols.contains("id")),
            "users.id must be in the materialisation plan; got {:?}",
            report.referenced,
        );
    }

    #[test]
    fn per_parent_owner_recorded_on_report() {
        let src = r#"
            output: sql
            table users { id: sequence }
            table posts {
              id: sequence
              author_id: ref(users.id, per_parent: 2..7)
            }
            generate users: 5
        "#;
        let file = parse_ok(src);
        let report = check(&file);
        assert!(report.is_ok(), "errors: {:?}", report.errors);
        let owner = report.per_parent_owners.get("posts").expect("posts is owned");
        assert_eq!(owner.0, "users");
        assert_eq!(owner.1, "id");
        assert_eq!(owner.2, (2, 7));
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
