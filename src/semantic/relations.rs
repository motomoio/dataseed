//! Phase 3 cross-table validation.
//!
//! Walks every `ref()` call in the AST and answers four questions:
//!
//! 1. Does the target table exist? → `UndeclaredRefTable`
//! 2. Does the target column exist in that table? → `UndeclaredRefColumn`
//! 3. Is the self-reference legal? → `IllegalSelfReference` if the target
//!    column itself depends on another column in the same row (cascading
//!    dependency). Self-refs to independent columns are allowed and recorded
//!    in `self_ref_tables` so the engine can switch on two-pass generation.
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
    /// Tables with at least one legal self-reference. The engine reads this
    /// to enable two-pass row generation for those tables.
    pub self_ref_tables: BTreeSet<String>,
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
                // Self-owning per_parent (e.g. `ref(employees.id, per_parent: ...)`
                // inside table `employees`) doesn't make sense — the parent
                // pool isn't materialised when the per_parent quota would be
                // drawn. Reject it cleanly at lint time rather than panicking
                // in the engine. The full self-reference branch below still
                // runs and may add its own diagnostics; that's intentional.
                if only.parent_table == table.name {
                    report.errors.push(SemanticError::IllegalSelfReference {
                        line: only.line,
                        col: only.col,
                        table: table.name.clone(),
                        column: only.parent_column.clone(),
                        reason: format!(
                            "self-references cannot use `per_parent` — the parent pool isn't materialised when the quota is drawn"
                        ),
                    });
                } else {
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
    // Each entry is one column reference: where it appears, and what it
    // points at. The walker finds `Value::ColumnRef` reachable from any
    // call — top-level `ref(T.C)`, inlined `ref(T.C)` collapsed to a bare
    // ColumnRef inside another generator's kwargs, or refs nested inside
    // arrays. Each such occurrence contributes one edge.
    //
    // Filtering happens here so per-table iteration is local. We
    // deliberately do NOT filter by `field.call.function == "ref"` —
    // Task 3.2 introduced nested refs (e.g.
    // `randomPointNear(center: ref(warehouses.location), ...)`), and the
    // dependency-graph walker must see those too or the topo sort and
    // materialisation plan miss the edge.
    let mut edges: Vec<CycleEdge> = Vec::new();
    for table in &file.tables {
        let table_name = table.name.as_str();
        for field in &table.fields {
            for_each_column_ref(&field.call, |parent_table, parent_column, line, col| {
                // 1) target table must exist
                if !declared.contains_key(parent_table) {
                    report.errors.push(SemanticError::UndeclaredRefTable {
                        line,
                        col,
                        table: parent_table.to_string(),
                    });
                    return;
                }

                // 2) target column must exist in that table
                let target_table = file
                    .tables
                    .iter()
                    .find(|t| t.name == parent_table)
                    .expect("declared but missing from tables?");
                let column_exists = target_table.fields.iter().any(|f| f.name == parent_column);
                if !column_exists {
                    report.errors.push(SemanticError::UndeclaredRefColumn {
                        line,
                        col,
                        table: parent_table.to_string(),
                        column: parent_column.to_string(),
                    });
                    return;
                }

                // 3) Self-references: allowed iff the target column is
                // "independent" — its own call contains no column refs
                // anywhere (no cascading dependencies). Otherwise reject.
                if table_name == parent_table {
                    let target_field = file
                        .tables
                        .iter()
                        .find(|t| t.name == parent_table)
                        .and_then(|t| t.fields.iter().find(|f| f.name == parent_column));
                    let target_uses_column_ref = match target_field {
                        Some(f) => call_has_any_column_ref(&f.call),
                        // Target doesn't exist; UndeclaredRefColumn already
                        // emitted above so we'd never reach here. Defensive.
                        None => false,
                    };
                    if target_uses_column_ref {
                        report.errors.push(SemanticError::IllegalSelfReference {
                            line,
                            col,
                            table: parent_table.to_string(),
                            column: parent_column.to_string(),
                            reason: format!(
                                "the target column `{parent_table}.{parent_column}` itself references another column"
                            ),
                        });
                    } else {
                        // Legal self-ref: record the table so the engine can
                        // enable two-pass generation for it, and add the
                        // target column to the materialisation plan so the
                        // pool retains its values for the same-table draw.
                        report.self_ref_tables.insert(table_name.to_string());
                        report
                            .referenced
                            .entry(parent_table.to_string())
                            .or_default()
                            .insert(parent_column.to_string());
                    }
                    // Intra-table edge; never contributes to the inter-table
                    // topo graph regardless of legality.
                    return;
                }

                edges.push(CycleEdge {
                    from_table: table_name.to_string(),
                    from_field: field.name.clone(),
                    to_table: parent_table.to_string(),
                    to_column: parent_column.to_string(),
                    line,
                    col,
                });
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
///
/// This helper is still used by the per_parent owner pass: only a
/// top-level `ref(T.C)` call can carry per_parent semantics, so the owner
/// pass keys on `function == "ref"` and uses this to pull out the target.
/// Nested refs (collapsed by the parser into bare `Value::ColumnRef`
/// inside another generator's kwargs) don't carry per_parent — they're
/// handled by [`for_each_column_ref`].
fn extract_target(call: &Call) -> Option<(String, String)> {
    if call.positional.len() != 1 {
        return None;
    }
    match &call.positional[0] {
        Value::ColumnRef { table, column } => Some((table.clone(), column.clone())),
        _ => None,
    }
}

/// Walk every `Value::ColumnRef` reachable from this call, including
/// those nested inside arrays or as kwarg values. Each one represents a
/// dependency edge from the field's table to `(table, column)`.
///
/// The callback receives the target `(table, column)` and the source
/// location of the enclosing call — Phase 1/2 didn't preserve per-value
/// line/col so we surface the call's position. That's accurate enough for
/// diagnostics because a single call can carry multiple refs but they all
/// share a textual neighbourhood.
fn for_each_column_ref<F>(call: &Call, mut f: F)
where
    F: FnMut(&str, &str, usize, usize),
{
    fn walk<F: FnMut(&str, &str, usize, usize)>(
        v: &Value,
        line: usize,
        col: usize,
        f: &mut F,
    ) {
        match v {
            Value::ColumnRef { table, column } => f(table, column, line, col),
            Value::Array(items) => {
                for it in items {
                    walk(it, line, col, f);
                }
            }
            _ => {}
        }
    }
    for p in &call.positional {
        walk(p, call.line, call.col, &mut f);
    }
    for (_, v) in &call.kwargs {
        walk(v, call.line, call.col, &mut f);
    }
}

/// `true` iff `call` (or any value reachable from it — positional args,
/// kwargs, nested array items) contains at least one `Value::ColumnRef`.
///
/// Used by the self-reference check to classify the target column as
/// "independent" (no column refs in its generator call) or "dependent"
/// (reads some other column in the same row, possibly transitively).
/// Phase 4.4 only permits self-refs to independent columns.
fn call_has_any_column_ref(call: &Call) -> bool {
    fn walk(v: &Value) -> bool {
        match v {
            Value::ColumnRef { .. } => true,
            Value::Array(items) => items.iter().any(walk),
            _ => false,
        }
    }
    call.positional.iter().any(walk) || call.kwargs.iter().any(|(_, v)| walk(v))
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
    fn self_reference_to_sequence_is_allowed() {
        let src = r#"
            output: sql
            table employees {
              id:         sequence
              manager_id: ref(employees.id)
            }
            generate employees: 100
        "#;
        let file = parse_ok(src);
        let report = check(&file);
        assert!(report.is_ok(), "errors: {:?}", report.errors);
        assert!(report.self_ref_tables.contains("employees"));
    }

    #[test]
    fn self_reference_to_dependent_column_rejected() {
        let src = r#"
            output: sql
            table employees {
              id:         sequence
              manager_id: ref(employees.id)
              buddy_id:   ref(employees.manager_id)
            }
            generate employees: 5
        "#;
        let file = parse_ok(src);
        let report = check(&file);
        assert!(!report.is_ok());
        let illegal = report
            .errors
            .iter()
            .find(|e| matches!(e, SemanticError::IllegalSelfReference { .. }));
        assert!(
            illegal.is_some(),
            "expected IllegalSelfReference, got: {:?}",
            report.errors
        );
    }

    #[test]
    fn self_reference_to_independent_random_generator_is_allowed() {
        let src = r#"
            output: sql
            table comments {
              id:        sequence
              parent_id: ref(comments.id)
              body:      randomWord()
            }
            generate comments: 10
        "#;
        let file = parse_ok(src);
        let report = check(&file);
        assert!(report.is_ok(), "errors: {:?}", report.errors);
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
