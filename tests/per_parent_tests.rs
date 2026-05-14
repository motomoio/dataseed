use dataseed::parser::parse;
use dataseed::rng::SeedRng;
use dataseed::output::{render_plan, RenderPlan};
use dataseed::semantic;
use std::collections::BTreeMap;

fn render_to_string(src: &str, seed: u64) -> String {
    let file = parse(src).expect("parse");
    let report = semantic::check(&file);
    assert!(report.is_ok(), "semantic errors: {:?}", report.errors);
    let counts: BTreeMap<String, u64> = file
        .generate
        .iter()
        .map(|g| (g.table.clone(), g.count))
        .collect();
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
fn per_parent_emits_quota_rows_per_parent() {
    let src = r#"
        output: sql
        table users {
          id: sequence
        }
        table posts {
          id: sequence
          author_id: ref(users.id, per_parent: 2..2)
        }
        generate users: 5
    "#;
    let out = render_to_string(src, 42);
    // 5 users × exactly 2 posts each = 10 INSERT rows. The filter counts
    // every INSERT row (lines starting with `(` after indent) regardless of
    // column count — single-column user rows look like `(1),`, multi-column
    // post rows look like `(1, 1),`.
    let total_rows = out
        .lines()
        .filter(|l| l.trim_start().starts_with("("))
        .count();
    assert_eq!(total_rows, 5 + 10, "5 user rows + 10 post rows");
}

#[test]
fn per_parent_uses_full_range() {
    let src = r#"
        output: sql
        table users { id: sequence }
        table posts {
          id: sequence
          author_id: ref(users.id, per_parent: 0..10)
        }
        generate users: 50
    "#;
    let out = render_to_string(src, 7);
    let post_rows = out.lines().filter(|l| l.starts_with("  (")).count();
    // 50 users × avg 5 posts ≈ 250 rows. Loose bounds to avoid flakiness.
    assert!(post_rows > 50, "got {post_rows}");
    assert!(post_rows < 500, "got {post_rows}");
}

#[test]
fn per_parent_assigns_exact_quota_per_parent_contiguously() {
    let src = r#"
        output: sql
        table parents {
          id: sequence
        }
        table kids {
          id: sequence
          parent_id: ref(parents.id, per_parent: 3..3)
        }
        generate parents: 4
    "#;
    let out = render_to_string(src, 1);

    // Each parent (1..=4) should appear exactly 3 times in kids' parent_id column.
    for parent in 1..=4 {
        let occurrences = out
            .lines()
            .filter(|l| l.contains(&format!(", {parent})")))
            .count();
        assert_eq!(occurrences, 3, "parent_id={parent} should appear 3 times, got {occurrences}");
    }

    // Engine layout is contiguous-block: parent 1's children come before parent 2's,
    // etc. Verify by extracting the parent_id sequence from kids' rows.
    let kids_section_start = out
        .find("INSERT INTO kids")
        .expect("INSERT INTO kids must appear");
    let kids_section = &out[kids_section_start..];
    let mut parent_seq: Vec<u64> = Vec::new();
    for line in kids_section.lines() {
        // Lines look like:  "  (1, 1)," — capture the second column.
        let trimmed = line.trim();
        if !trimmed.starts_with("(") { continue; }
        // strip leading "(" and trailing ")," or ");"
        let inner = trimmed.trim_start_matches('(').trim_end_matches(',').trim_end_matches(';').trim_end_matches(')');
        let parts: Vec<&str> = inner.split(", ").collect();
        if parts.len() != 2 { continue; }
        if let Ok(pid) = parts[1].parse::<u64>() {
            parent_seq.push(pid);
        }
    }
    assert_eq!(parent_seq.len(), 12, "expected 12 kids rows, got {}", parent_seq.len());
    // Contiguous: rows 0..3 have parent_id 1, rows 3..6 have parent_id 2, etc.
    for parent in 1..=4u64 {
        for i in 0..3 {
            let idx = (parent as usize - 1) * 3 + i;
            assert_eq!(parent_seq[idx], parent, "kid row {idx} should have parent_id={parent}, got {}", parent_seq[idx]);
        }
    }
}

#[test]
fn per_parent_does_not_force_unrelated_refs() {
    // A child table has two refs: one with per_parent (to parents), one plain (to categories).
    // The per_parent quota must drive child counts and force parents.id assignment,
    // but the plain ref must keep uniform-random behaviour against categories.
    let src = r#"
        output: sql
        table parents {
          id: sequence
        }
        table categories {
          id: sequence
        }
        table kids {
          id:          sequence
          parent_id:   ref(parents.id, per_parent: 5..5)
          category_id: ref(categories.id)
        }
        generate parents: 4
        generate categories: 10
    "#;
    let out = render_to_string(src, 2026);

    // 4 parents × 5 kids each = 20 kids rows.
    let kids_section_start = out
        .find("INSERT INTO kids")
        .expect("INSERT INTO kids must appear");
    let kids_section = &out[kids_section_start..];

    let mut categories_seen: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    let mut row_count = 0;
    for line in kids_section.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("(") { continue; }
        let inner = trimmed.trim_start_matches('(').trim_end_matches(',').trim_end_matches(';').trim_end_matches(')');
        let parts: Vec<&str> = inner.split(", ").collect();
        if parts.len() != 3 { continue; }
        row_count += 1;
        if let Ok(cat) = parts[2].parse::<u64>() {
            categories_seen.insert(cat);
        }
    }
    assert_eq!(row_count, 20, "expected 20 kids rows");
    // The plain ref to categories should sample more than one value across 20 rows
    // (if it were "forced" to a single index, only one category would appear).
    assert!(
        categories_seen.len() > 1,
        "expected plain ref to categories to be uniformly random across 20 rows, only saw {:?}",
        categories_seen
    );
}
