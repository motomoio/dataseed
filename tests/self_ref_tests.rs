use dataseed::output::{render_plan, RenderPlan};
use dataseed::parser::parse;
use dataseed::rng::SeedRng;
use dataseed::semantic;
use std::collections::BTreeMap;

fn render(src: &str, seed: u64) -> String {
    let file = parse(src).expect("parse");
    let report = semantic::check(&file);
    assert!(report.is_ok(), "semantic errors: {:?}", report.errors);
    let counts: BTreeMap<String, u64> =
        file.generate.iter().map(|g| (g.table.clone(), g.count)).collect();
    let plan = RenderPlan {
        topo_order: report.topo_order.clone(),
        referenced: report.referenced.clone(),
        counts,
        emit_only: None,
        per_parent_owners: report.per_parent_owners.clone(),
        self_ref_tables: report.self_ref_tables.clone(),
        emit_ddl: false,
    };
    let mut rng = SeedRng::from_seed(seed);
    let mut buf: Vec<u8> = Vec::new();
    render_plan(&file, &plan, &mut rng, &mut buf).expect("render");
    String::from_utf8(buf).unwrap()
}

#[test]
fn manager_id_always_references_an_existing_employee_id() {
    let src = r#"
        output: sql
        table employees {
          id:         sequence
          manager_id: ref(employees.id)
        }
        generate employees: 100
    "#;
    let out = render(src, 42);

    // Every (id, manager_id) row should have manager_id in 1..=100.
    let start = out.find("INSERT INTO employees").expect("employees section");
    let section = &out[start..];

    let mut rows = Vec::new();
    for line in section.lines() {
        let t = line.trim();
        if !t.starts_with('(') { continue; }
        let inner = t.trim_start_matches('(').trim_end_matches(',').trim_end_matches(';').trim_end_matches(')');
        let parts: Vec<&str> = inner.split(", ").collect();
        if parts.len() != 2 { continue; }
        let id: u64 = parts[0].parse().expect("id");
        let mgr: u64 = parts[1].parse().expect("manager_id");
        rows.push((id, mgr));
    }
    assert_eq!(rows.len(), 100, "expected 100 rows, got {}", rows.len());
    let ids: std::collections::BTreeSet<u64> = rows.iter().map(|(i, _)| *i).collect();
    for (id, mgr) in &rows {
        assert!(ids.contains(mgr), "row id={id} has manager_id={mgr} which doesn't exist");
    }
}

#[test]
fn self_ref_doesnt_affect_files_without_it() {
    // Plain table without self-refs — should produce same output as before
    // Phase 4.4 work touched the engine.
    let src = r#"
        output: sql
        table users {
          id:   sequence
          name: randomName()
        }
        generate users: 10
    "#;
    let out = render(src, 42);
    assert!(out.contains("INSERT INTO users"));
    let row_count = out.lines().filter(|l| l.trim_start().starts_with('(')).count();
    assert_eq!(row_count, 10);
}

#[test]
fn self_ref_draws_from_all_rows_not_just_earlier_ones() {
    // Strong test: confirms row 0's self-ref can draw from later rows.
    // With 1000 rows and seed=42, some early rows MUST manager_id > their own id.
    let src = r#"
        output: sql
        table employees {
          id:         sequence
          manager_id: ref(employees.id)
        }
        generate employees: 1000
    "#;
    let out = render(src, 42);
    let start = out.find("INSERT INTO employees").expect("employees section");
    let section = &out[start..];

    let mut found_later_manager = false;
    for line in section.lines() {
        let t = line.trim();
        if !t.starts_with('(') { continue; }
        let inner = t.trim_start_matches('(').trim_end_matches(',').trim_end_matches(';').trim_end_matches(')');
        let parts: Vec<&str> = inner.split(", ").collect();
        if parts.len() != 2 { continue; }
        let id: u64 = parts[0].parse().expect("id");
        let mgr: u64 = parts[1].parse().expect("manager_id");
        if mgr > id {
            found_later_manager = true;
            break;
        }
    }
    assert!(
        found_later_manager,
        "expected at least one row where manager_id > id (self-ref must see future rows)"
    );
}
