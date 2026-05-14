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
fn per_parent_assigns_child_rows_round_robin_within_quota() {
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
}
