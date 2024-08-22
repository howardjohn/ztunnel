use std::time::Duration;

use bytes::Bytes;
use criterion::{criterion_group, criterion_main, Criterion};
use pprof::criterion::{Output, PProfProfiler};

use ztunnel::state::ProxyState;
use ztunnel::xds::ProxyStateUpdateMutator;

pub fn xds(c: &mut Criterion) {
    use ztunnel::xds::istio::workload::Port;
    use ztunnel::xds::istio::workload::Service as XdsService;
    use ztunnel::xds::istio::workload::Workload as XdsWorkload;
    use ztunnel::xds::istio::workload::{NetworkAddress as XdsNetworkAddress, PortList};
    let mut c = c.benchmark_group("xds");
    // let updater = ProxyStateUpdater::new(state.clone(), Arc::new(ztunnel::cert_fetcher::NoCertFetcher()));
    let mut state = ProxyState::default();
    let updater = ProxyStateUpdateMutator::new_no_fetch();
    let svc = XdsService {
        hostname: "example.com".to_string(),
        addresses: vec![XdsNetworkAddress {
            network: "".to_string(),
            address: vec![127, 0, 0, 3],
        }],
        ..Default::default()
    };
    updater.insert_service(&mut state, svc).unwrap();

    c.measurement_time(Duration::from_secs(5));
    c.bench_function("insert-remove", |b| {
        b.iter(|| {
            let svc = XdsService {
                hostname: "example.com".to_string(),
                addresses: vec![XdsNetworkAddress {
                    network: "".to_string(),
                    address: vec![127, 0, 0, 3],
                }],
                ..Default::default()
            };
            let mut state = ProxyState::default();
            let updater = ProxyStateUpdateMutator::new_no_fetch();
            updater.insert_service(&mut state, svc).unwrap();
            const WORKLOAD_COUNT: usize = 500;
            for i in 0..WORKLOAD_COUNT {
                updater
                    .insert_workload(
                        &mut state,
                        XdsWorkload {
                            uid: format!("cluster1//v1/Pod/default/{i}"),
                            addresses: vec![Bytes::copy_from_slice(&[
                                127,
                                0,
                                (i / 255) as u8,
                                (i % 255) as u8,
                            ])],
                            services: std::collections::HashMap::from([(
                                "/example.com".to_string(),
                                PortList {
                                    ports: vec![Port {
                                        service_port: 80,
                                        target_port: 1234,
                                    }],
                                },
                            )]),
                            ..Default::default()
                        },
                    )
                    .unwrap();
            }
        })
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .with_profiler(PProfProfiler::new(100, Output::Protobuf))
        .warm_up_time(Duration::from_millis(1));
    targets = xds
}

criterion_main!(benches);
