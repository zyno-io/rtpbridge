use criterion::{Criterion, criterion_group, criterion_main};
use rtpbridge::control::protocol::{EndpointDirection, EndpointId};
use rtpbridge::session::routing::RoutingTable;
use uuid::Uuid;

/// Build a list of N endpoints, cycling through directions.
fn make_endpoints(n: usize) -> Vec<(EndpointId, EndpointDirection, bool)> {
    let directions = [
        EndpointDirection::SendRecv,
        EndpointDirection::SendOnly,
        EndpointDirection::RecvOnly,
    ];
    (0..n)
        .map(|i| (Uuid::new_v4(), directions[i % directions.len()], false))
        .collect()
}

fn bench_rebuild_2(c: &mut Criterion) {
    let endpoints = make_endpoints(2);
    let mut table = RoutingTable::new();
    c.bench_function("routing_rebuild_2_endpoints", |b| {
        b.iter(|| {
            table.rebuild(&endpoints);
        })
    });
}

fn bench_rebuild_10(c: &mut Criterion) {
    let endpoints = make_endpoints(10);
    let mut table = RoutingTable::new();
    c.bench_function("routing_rebuild_10_endpoints", |b| {
        b.iter(|| {
            table.rebuild(&endpoints);
        })
    });
}

fn bench_rebuild_50(c: &mut Criterion) {
    let endpoints = make_endpoints(50);
    let mut table = RoutingTable::new();
    c.bench_function("routing_rebuild_50_endpoints", |b| {
        b.iter(|| {
            table.rebuild(&endpoints);
        })
    });
}

fn bench_rebuild_100(c: &mut Criterion) {
    let endpoints = make_endpoints(100);
    let mut table = RoutingTable::new();
    c.bench_function("routing_rebuild_100_endpoints", |b| {
        b.iter(|| {
            table.rebuild(&endpoints);
        })
    });
}

fn bench_destinations_lookup(c: &mut Criterion) {
    let endpoints = make_endpoints(10);
    let mut table = RoutingTable::new();
    table.rebuild(&endpoints);
    // Pick a SendRecv endpoint (index 0 is always SendRecv)
    let source = endpoints[0].0;
    c.bench_function("routing_destinations_lookup", |b| {
        b.iter(|| {
            let _dests = table.destinations(&source);
        })
    });
}

fn bench_fanout_1_to_10(c: &mut Criterion) {
    // 1 SendOnly sender + 10 RecvOnly receivers
    let mut endpoints = Vec::new();
    let sender = Uuid::new_v4();
    endpoints.push((sender, EndpointDirection::SendOnly, false));
    for _ in 0..10 {
        endpoints.push((Uuid::new_v4(), EndpointDirection::RecvOnly, false));
    }

    let mut table = RoutingTable::new();
    table.rebuild(&endpoints);

    c.bench_function("routing_fanout_1_to_10", |b| {
        b.iter(|| {
            let _dests = table.destinations(&sender);
        })
    });
}

fn bench_fanout_1_to_50(c: &mut Criterion) {
    let mut endpoints = Vec::new();
    let sender = Uuid::new_v4();
    endpoints.push((sender, EndpointDirection::SendOnly, false));
    for _ in 0..50 {
        endpoints.push((Uuid::new_v4(), EndpointDirection::RecvOnly, false));
    }

    let mut table = RoutingTable::new();
    table.rebuild(&endpoints);

    c.bench_function("routing_fanout_1_to_50", |b| {
        b.iter(|| {
            let _dests = table.destinations(&sender);
        })
    });
}

criterion_group!(
    benches,
    bench_rebuild_2,
    bench_rebuild_10,
    bench_rebuild_50,
    bench_rebuild_100,
    bench_destinations_lookup,
    bench_fanout_1_to_10,
    bench_fanout_1_to_50,
);
criterion_main!(benches);
