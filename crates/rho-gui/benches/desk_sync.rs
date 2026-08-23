//! Benchmarks the pure per-frame dashboard pass: parsing the desk
//! document, generating the listing, and comparing it against the
//! previous pass. `refresh_dashboard` pays this on every render, so it
//! must stay far under a frame budget.

use std::collections::HashMap;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use rho_core::UnixMs;
use rho_gui::dashboard::bench_support::{Pass, generate_pass};
use rho_gui::registry::{AgentRegistry, HostId};
use rho_ui_proto::desk::parse;
use rho_ui_proto::{
    AgentDisposition, AgentId, AgentIdDomain, AgentRole, UiAgentSummary, UiAttention, WorkspaceInfo,
};

struct Fixture {
    registry: AgentRegistry,
    documents: Vec<(HostId, String)>,
    filed: HashMap<(HostId, usize), Vec<AgentId>>,
}

fn agent_summary(id: u64) -> UiAgentSummary {
    UiAgentSummary {
        agent_id: AgentId::from_counter(id, &AgentIdDomain(0)).unwrap(),
        parent_agent: None,
        display_name: Some(format!("agent-{id}")),
        created_at: UnixMs(id),
        updated_at: UnixMs(id),
        role: AgentRole::default(),
        workspace: WorkspaceInfo::UserCheckout {
            repo: "/tmp".into(),
        },
        attention: UiAttention::Quiet,
        last_active: UnixMs(id),
        facts: Default::default(),
        hidden: false,
        disposition: AgentDisposition::Pending,
        last_user_message_text: "a recent user message that shows up as the row snippet".to_owned(),
        activity: None,
        turn_report: None,
        labels: Vec::new(),
    }
}

fn build_fixture(heading_count: usize) -> Fixture {
    let host = HostId::default();
    let mut registry = AgentRegistry::default();
    registry.attach_host(host, "local".to_owned());
    let agents: Vec<UiAgentSummary> = (1..=heading_count as u64 * 2).map(agent_summary).collect();
    for summary in &agents {
        registry.note_agent_created(host, summary.agent_id);
    }
    registry.set_host_data(host, 0, agents.len() as u64, agents.clone());

    let mut text = String::new();
    for index in 0..heading_count {
        text.push_str(&format!("* TODO Topic number {index}\n"));
        text.push_str(":project: rho\n");
        text.push_str(&format!(
            "a body line describing topic {index} in enough detail to wrap\n"
        ));
    }
    // File two agents under every heading, leaving none unfiled.
    let mut filed = HashMap::new();
    for (index, heading) in parse(&text).iter().enumerate() {
        filed.insert(
            (host, heading.heading_range.start),
            vec![agents[index * 2].agent_id, agents[index * 2 + 1].agent_id],
        );
    }
    Fixture {
        registry,
        documents: vec![(host, text)],
        filed,
    }
}

fn desk_sync_benchmarks(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("desk per-frame pass");
    for heading_count in [10usize, 50, 150] {
        let fixture = build_fixture(heading_count);
        let previous = generate_pass(&fixture.registry, &fixture.documents, &fixture.filed);

        group.bench_with_input(
            BenchmarkId::new("parse", heading_count),
            &fixture,
            |bench, fixture| bench.iter(|| parse(&fixture.documents[0].1).len()),
        );
        group.bench_with_input(
            BenchmarkId::new("generate", heading_count),
            &fixture,
            |bench, fixture| {
                bench.iter(|| {
                    generate_pass(&fixture.registry, &fixture.documents, &fixture.filed).len()
                })
            },
        );
        group.bench_with_input(
            BenchmarkId::new("generate+compare", heading_count),
            &(&fixture, &previous),
            |bench, (fixture, previous)| {
                bench.iter(|| {
                    let pass: Pass =
                        generate_pass(&fixture.registry, &fixture.documents, &fixture.filed);
                    assert!(pass.matches(previous));
                })
            },
        );
    }
    group.finish();
}

criterion_group!(benches, desk_sync_benchmarks);
criterion_main!(benches);
