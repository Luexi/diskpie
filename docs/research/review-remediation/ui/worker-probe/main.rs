use std::{sync::Arc, thread, time::{Duration, Instant}};
use diskpie_app::layout_service::LayoutService;
use diskpie_core::{GenerationId, MetricSource, NodeSpec, OwnMetrics, SizeMetric, TreeBuilder};
use diskpie_core::sunburst::{HiddenBranches, LayoutOptions};

fn main() {
    let args: Vec<_> = std::env::args().collect();
    let variant = &args[1];
    let run = &args[2];
    let mut builder = TreeBuilder::new(GenerationId::new(1));
    let root = builder.add_root(NodeSpec::root(r"C:\DiskPie-in-memory-worker-probe")).unwrap();
    let mut random = 42_u64;
    for index in 0..1_000_000 {
        random ^= random << 13; random ^= random >> 7; random ^= random << 17;
        let size = SizeMetric::known(random % 100_000 + 1, MetricSource::PortableMetadata);
        builder.add_child(root, NodeSpec::file(format!("file-{index:08}"), OwnMetrics::new(size, size))).unwrap();
    }
    let small_root = builder.add_child(root, NodeSpec::directory("small")).unwrap();
    let size = SizeMetric::known(1, MetricSource::PortableMetadata);
    builder.add_child(small_root, NodeSpec::file("leaf", OwnMetrics::new(size, size))).unwrap();
    let snapshot = Arc::new(builder.freeze().unwrap());
    println!("variant,run,phase,was_busy_before_request,elapsed_ms");
    for phase in ["supersede_to_small_completion", "shutdown_drop_to_join"] {
        let mut service = LayoutService::new().unwrap();
        service.submit(Arc::clone(&snapshot), LayoutOptions::new(root), Arc::new(HiddenBranches::new())).unwrap();
        // A fixed delay exercises a running worker. It is not a test barrier
        // inside production sorting and does not claim a worst-case phase.
        thread::sleep(Duration::from_millis(200));
        let was_busy = service.is_busy();
        let started = Instant::now();
        if phase == "shutdown_drop_to_join" {
            drop(service); // LayoutService::Drop requests stop and actually joins.
            println!("{variant},{run},{phase},{was_busy},{:.6}", started.elapsed().as_secs_f64()*1000.0);
        } else {
            let next = service.submit(Arc::clone(&snapshot), LayoutOptions::new(small_root), Arc::new(HiddenBranches::new())).unwrap();
            loop {
                if let Some(completion) = service.try_take() {
                    assert_eq!(completion.id, next, "superseded work cannot publish after admission");
                    let layout = completion.result.unwrap();
                    assert_eq!(layout.root(), small_root);
                    println!("{variant},{run},{phase},{was_busy},{:.6}", started.elapsed().as_secs_f64()*1000.0);
                    break;
                }
                assert!(started.elapsed() < Duration::from_secs(10), "worker did not hand off");
                thread::sleep(Duration::from_millis(1));
            }
            drop(service);
        }
    }
}
