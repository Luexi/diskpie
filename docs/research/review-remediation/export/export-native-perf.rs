use diskpie_app::diagnostics::{
    AggregateMetrics, AllowlistedSettings, AppVersion, Architecture, CoarseOsVersion,
    DiagnosticCounters, ExportMetadata, ExportRequest, LogSource, OperatingSystem, Target,
    EXPORT_SCHEMA_VERSION, build_export,
};
use std::{fs, path::Path, time::Instant};

fn main() {
    let args: Vec<_> = std::env::args().collect();
    let mode = args.get(1).expect("mode");
    let directory = Path::new(args.get(2).expect("fixture directory"));
    if mode == "setup-event" || mode == "setup-blank" {
        fs::create_dir(directory).expect("fixture directory must be new");
        let record = if mode == "setup-blank" { "\n" } else {
            "2026-09-07T00:00:00Z INFO diskpie event=scan.progress generation=1 file_count=100000 directory_count=100 omission_count=0\n"
        };
        let lines = if mode == "setup-blank" { 131_072 } else { 8_125 };
        let bytes = record.repeat(lines);
        assert!(bytes.len() <= 1024 * 1024);
        for day in 1..=8 {
            fs::write(directory.join(format!("diskpie.2026-09-{day:02}.log")), &bytes).unwrap();
        }
        return;
    }
    let snapshot = diskpie_platform::windows::diagnostic_files::collect_diagnostic_logs(directory).unwrap();
    assert_eq!(snapshot.omitted_files, 0);
    assert_eq!(snapshot.omitted_bytes, 0);
    let storage: Vec<_> = snapshot.sources.into_iter().enumerate().map(|(index, bytes)| {
        let bytes = bytes.expect("safe native fixture source");
        assert!(bytes.len() <= 1024 * 1024 && bytes.ends_with(b"\n"));
        assert_eq!(bytes, fs::read(directory.join(format!("diskpie.2026-09-{:02}.log", index + 1))).unwrap());
        bytes
    }).collect();
    let sources: Vec<_> = storage.iter().map(|bytes| LogSource::available(bytes)).collect();
    let request = ExportRequest {
        metadata: ExportMetadata {
            app_version: AppVersion::new(0, 1, 0),
            target: Target::new(OperatingSystem::Windows, Architecture::X86_64),
            coarse_os_version: CoarseOsVersion::default(),
            panic_marker_present: false,
        },
        settings: AllowlistedSettings::default(),
        aggregates: AggregateMetrics::default(),
        counters: DiagnosticCounters::default(),
        log_sources: &sources,
        include_paths: false,
    };
    let started = Instant::now();
    let export = build_export(&request);
    let elapsed = started.elapsed();
    let expected: Vec<u8> = storage.iter().flatten().copied().collect();
    let recent = export.artifact().text().split_once("[recent_logs]\n").unwrap().1;
    assert_eq!(recent.as_bytes(), expected);
    assert_eq!(export.stats().included_source_files, 8);
    assert_eq!(export.stats().omitted_lines, 0);
    assert!(export.preview_is_exact_artifact());
    println!("schema={} sources={} input_bytes={} lines={} artifact_bytes={} elapsed_ms={:.3}",
        EXPORT_SCHEMA_VERSION, sources.len(), expected.len(), export.stats().included_lines,
        export.artifact().bytes().len(), elapsed.as_secs_f64() * 1000.0);
}
