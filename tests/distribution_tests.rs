use dataseed::output::{render_plan, RenderPlan};
use dataseed::parser::parse;
use dataseed::rng::SeedRng;
use dataseed::semantic;
use std::collections::BTreeMap;

fn render_to_string(src: &str, seed: u64) -> String {
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
    };
    let mut rng = SeedRng::from_seed(seed);
    let mut buf: Vec<u8> = Vec::new();
    render_plan(&file, &plan, &mut rng, &mut buf).expect("render");
    String::from_utf8(buf).unwrap()
}

#[test]
fn zipf_ref_heavily_favours_first_rank() {
    let src = r#"
        output: sql
        table users { id: sequence }
        table actions {
          id:      sequence
          user_id: ref(users.id, distribution: "zipf")
        }
        generate users: 100
        generate actions: 10000
    "#;
    let out = render_to_string(src, 42);

    // Count occurrences of "user 1" vs "user 100" in actions' user_id column.
    // Each actions INSERT row looks like:  "  (NNN, 1)," — user_id is the
    // last value before `)`. Build a small parser to extract user_id.
    let actions_start = out.find("INSERT INTO actions").expect("actions section");
    let actions_section = &out[actions_start..];

    let mut hist = std::collections::BTreeMap::<u64, u64>::new();
    for line in actions_section.lines() {
        let t = line.trim();
        if !t.starts_with('(') { continue; }
        // strip leading "(", trailing ")," or ");"
        let inner = t.trim_start_matches('(').trim_end_matches(',').trim_end_matches(';').trim_end_matches(')');
        let parts: Vec<&str> = inner.split(", ").collect();
        if parts.len() != 2 { continue; }
        if let Ok(uid) = parts[1].parse::<u64>() {
            *hist.entry(uid).or_default() += 1;
        }
    }

    let c1 = *hist.get(&1).unwrap_or(&0);
    let c100 = *hist.get(&100).unwrap_or(&0);
    assert!(
        c1 > c100 * 10,
        "zipf should heavily favour user 1: c1={c1} c100={c100} hist[0..5]={:?}",
        hist.iter().take(5).collect::<Vec<_>>()
    );
}

#[test]
fn uniform_ref_is_balanced() {
    // Sanity: same setup with default uniform should be approximately flat.
    let src = r#"
        output: sql
        table users { id: sequence }
        table actions {
          id:      sequence
          user_id: ref(users.id)
        }
        generate users: 100
        generate actions: 10000
    "#;
    let out = render_to_string(src, 42);
    let actions_start = out.find("INSERT INTO actions").expect("actions section");
    let actions_section = &out[actions_start..];

    let mut hist = std::collections::BTreeMap::<u64, u64>::new();
    for line in actions_section.lines() {
        let t = line.trim();
        if !t.starts_with('(') { continue; }
        let inner = t.trim_start_matches('(').trim_end_matches(',').trim_end_matches(';').trim_end_matches(')');
        let parts: Vec<&str> = inner.split(", ").collect();
        if parts.len() != 2 { continue; }
        if let Ok(uid) = parts[1].parse::<u64>() {
            *hist.entry(uid).or_default() += 1;
        }
    }

    // 100 users × 10k actions → ~100 per user on average. Uniform should not
    // skew like zipf (c1 should NOT dominate).
    let c1 = *hist.get(&1).unwrap_or(&0);
    let c100 = *hist.get(&100).unwrap_or(&0);
    // c1 and c100 should be within a factor of 5 of each other — much
    // tighter than zipf's 10×+ separation.
    assert!(
        c1 < c100 * 5 && c100 < c1 * 5,
        "uniform should be roughly balanced: c1={c1} c100={c100}"
    );
}
