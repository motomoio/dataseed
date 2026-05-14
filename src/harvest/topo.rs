//! Topological sort of harvested tables by their FK graph.
//!
//! Parents (FK targets) must come before children in the emitted file so
//! `dataseed plant` can resolve `ref()` lookups in a single pass.
//!
//! Cycles are rare but possible (self-FKs, mutually-FK'd tables). When one
//! is detected we fall back to alphabetical order and surface a top-of-file
//! warning. The semantic check inside dataseed already handles legal self-
//! refs separately — we don't try to be clever about them here.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::harvest::model::HarvestSchema;

#[derive(Debug, Clone)]
pub struct TopoOutput {
    pub order: Vec<String>,
    /// `true` when a cycle was detected and the order is alphabetical.
    pub has_cycle: bool,
}

pub fn sort(schema: &HarvestSchema) -> TopoOutput {
    let all: BTreeSet<&str> = schema.tables.iter().map(|t| t.name.as_str()).collect();
    let mut adj: BTreeMap<&str, BTreeSet<&str>> = all.iter().map(|n| (*n, BTreeSet::new())).collect();
    let mut indeg: BTreeMap<&str, usize> = all.iter().map(|n| (*n, 0)).collect();

    for table in &schema.tables {
        for fk in &table.foreign_keys {
            // Skip self-FKs — they don't constrain ordering.
            if fk.ref_table == table.name {
                continue;
            }
            // Skip references to tables not in the harvested set.
            if !all.contains(fk.ref_table.as_str()) {
                continue;
            }
            // Edge: ref_table → table (parent before child).
            let parent: &str = fk.ref_table.as_str();
            let child: &str = table.name.as_str();
            // Use the BTreeSet to avoid double-counting if the same FK
            // appears in multiple columns of the same composite key.
            let inserted = adj
                .entry(parent)
                .or_insert_with(BTreeSet::new)
                .insert(child);
            if inserted {
                *indeg.entry(child).or_insert(0) += 1;
            }
        }
    }

    // Kahn with alphabetical tie-breaking (BTreeSet iteration is sorted).
    let mut order = Vec::with_capacity(schema.tables.len());
    let mut queue: VecDeque<&str> = indeg
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(n, _)| *n)
        .collect();
    while let Some(n) = queue.pop_front() {
        order.push(n.to_string());
        if let Some(children) = adj.get(n) {
            for c in children {
                let d = indeg.get_mut(c).unwrap();
                *d -= 1;
                if *d == 0 {
                    // Push at the end; the initial sort is alphabetical and
                    // every drain happens in a single pass — children of
                    // alphabetically-earlier parents land alphabetically too.
                    queue.push_back(c);
                }
            }
        }
    }

    if order.len() != schema.tables.len() {
        // Cycle: emit alphabetical and warn.
        let mut alpha: Vec<String> = all.iter().map(|s| s.to_string()).collect();
        alpha.sort();
        return TopoOutput {
            order: alpha,
            has_cycle: true,
        };
    }

    TopoOutput {
        order,
        has_cycle: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harvest::model::{ForeignKey, HarvestTable, SamplingStrategy, SourceInfo};
    use chrono::Utc;

    fn schema(tables: Vec<HarvestTable>) -> HarvestSchema {
        HarvestSchema {
            source: SourceInfo {
                database: "test".into(),
                schema: "public".into(),
                harvested_at: Utc::now(),
                invocation: String::new(),
            },
            tables,
            geometry_supported: false,
        }
    }

    fn t(name: &str, fks: Vec<(&str, &str)>) -> HarvestTable {
        HarvestTable {
            name: name.to_string(),
            columns: vec![],
            primary_key: vec![],
            foreign_keys: fks
                .into_iter()
                .enumerate()
                .map(|(i, (rt, rc))| ForeignKey {
                    constraint_name: format!("fk_{i}"),
                    columns: vec![format!("col_{i}")],
                    ref_table: rt.to_string(),
                    ref_columns: vec![rc.to_string()],
                })
                .collect(),
            estimated_rows: 0,
            sampling: SamplingStrategy::NoSample,
        }
    }

    #[test]
    fn parents_come_before_children() {
        let s = schema(vec![
            t("orders", vec![("users", "id")]),
            t("users", vec![]),
        ]);
        let r = sort(&s);
        assert!(!r.has_cycle);
        let users_idx = r.order.iter().position(|n| n == "users").unwrap();
        let orders_idx = r.order.iter().position(|n| n == "orders").unwrap();
        assert!(users_idx < orders_idx);
    }

    #[test]
    fn cycle_falls_back_to_alphabetical() {
        let s = schema(vec![
            t("a", vec![("b", "id")]),
            t("b", vec![("a", "id")]),
        ]);
        let r = sort(&s);
        assert!(r.has_cycle);
        assert_eq!(r.order, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn self_fk_is_ignored() {
        // A table that FK's to itself doesn't introduce a cycle in the
        // topological sense; it's still emittable.
        let s = schema(vec![t("comments", vec![("comments", "id")])]);
        let r = sort(&s);
        assert!(!r.has_cycle);
        assert_eq!(r.order, vec!["comments".to_string()]);
    }

    #[test]
    fn alphabetical_tie_break_within_same_level() {
        let s = schema(vec![t("zebra", vec![]), t("apple", vec![])]);
        let r = sort(&s);
        assert_eq!(r.order, vec!["apple".to_string(), "zebra".to_string()]);
    }
}
