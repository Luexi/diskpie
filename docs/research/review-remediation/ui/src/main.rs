use std::{hint::black_box, sync::Arc, time::Instant};
use diskpie_app::navigation::{NavigationAction, NavigationState};
use diskpie_core::{GenerationId, TreeBuilder, NodeSpec, OwnMetrics, SizeMetric, MetricSource};
use diskpie_core::sunburst::{LayoutOptions, HiddenBranches, compute_layout_cancellable};

fn main() {
    println!("scenario,entries,sample,milliseconds,checkpoint_count");
    eprintln!("AggregateSize={} NodeRecord={}", std::mem::size_of::<diskpie_core::AggregateSize>(),std::mem::size_of::<diskpie_core::NodeRecord>());
    for n in [100_000usize, 1_000_000] {
        let mut builder = TreeBuilder::new(GenerationId::new(1));
        let root = builder.add_root(NodeSpec::root(r"C:\DiskPie-review-in-memory")).unwrap();
        let mut state = 42_u64;
        for i in 0..n {
            state ^= state << 13; state ^= state >> 7; state ^= state << 17;
            let bytes = state % 100_000 + 1;
            let size = SizeMetric::known(bytes, MetricSource::PortableMetadata);
            builder.add_child(root, NodeSpec::file(format!("file-{i:08}"), OwnMetrics::new(size, size))).unwrap();
        }
        let freeze_start = Instant::now();
        let snapshot = Arc::new(builder.freeze().unwrap());
        println!("freeze,{n},0,{:.6},0", freeze_start.elapsed().as_secs_f64()*1000.0);
        let navigation = NavigationState::new(Arc::clone(&snapshot)).unwrap();
        for sample in 0..41 {
            let start = Instant::now();
            black_box(navigation.availability(black_box(navigation.command(NavigationAction::RescanAll))));
            println!("rescan_availability,{n},{sample},{:.6},0", start.elapsed().as_secs_f64()*1000.0);
        }
        for sample in 0..3 {
            let mut checkpoints = Vec::new();
            let start = Instant::now();
            let layout = compute_layout_cancellable(&snapshot, LayoutOptions::new(root), &HiddenBranches::new(), || {
                checkpoints.push(Instant::now()); false
            }).unwrap().unwrap();
            let overall = start.elapsed().as_secs_f64()*1000.0;
            let max_gap = checkpoints.windows(2).map(|w| w[1].duration_since(w[0])).max().unwrap();
            println!("flat_layout,{n},{sample},{overall:.6},{}", checkpoints.len());
            println!("flat_max_checkpoint_gap,{n},{sample},{:.6},{}", max_gap.as_secs_f64()*1000.0, layout.sectors().len());
        }
    }
}
