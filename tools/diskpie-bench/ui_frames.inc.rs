// Included only inside shell::tests, unchanged in both benchmark revisions.
mod benchmark {
    use super::*;
    use std::io::Write;

    struct FlatFs(usize);

    impl ScanFs for FlatFs {
        fn visit_directory(
            &self,
            _directory: &Path,
            visitor: &mut dyn FnMut(diskpie_scan::DirectoryItem) -> diskpie_scan::VisitControl,
        ) -> Result<(), diskpie_scan::FsError> {
            use diskpie_core::{EntryKind, MetricSource, OwnMetrics, SizeMetric};
            use diskpie_scan::{DirectoryItem, FsEntry, VisitControl};
            for index in 0..self.0 {
                let metrics = OwnMetrics::new(
                    SizeMetric::known((index % 10_000 + 1) as u64, MetricSource::PortableMetadata),
                    SizeMetric::known(
                        ((index % 10_000 / 4_096 + 1) * 4_096) as u64,
                        MetricSource::FilesystemAllocation,
                    ),
                );
                if visitor(DirectoryItem::Entry(FsEntry::new(
                    format!("file-{index:08}.dat"),
                    EntryKind::File,
                    metrics,
                ))) == VisitControl::Stop
                {
                    break;
                }
            }
            Ok(())
        }
    }

    struct FrameSample {
        stage: &'static str,
        index: usize,
        elapsed_us: f64,
        revision: u64,
        nodes: usize,
        ranked_rows: usize,
        shapes: usize,
    }

    fn frame(
        shell: &mut DiskPieShell,
        ctx: &egui::Context,
        stage: &'static str,
        index: usize,
        metric: Option<SizeBasis>,
    ) -> FrameSample {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 800.0),
            )),
            ..Default::default()
        };
        let started = Instant::now();
        let output = ctx.run_ui(input, |ui| {
            // Matches App::ui except the native owner HWND and close request.
            if let Some(metric) = metric {
                shell.dispatch(ShellCommand::SetMetric(metric));
            }
            let ctx = ui.ctx().clone();
            shell.logic(&ctx);
            shell.refresh_frame_caches();
            shell.handle_shortcuts(&ctx);
            shell.command_rail(ui);
            shell.telemetry_rail(ui);
            shell.synchronize_settings();
            shell.status_rail(ui);
            shell.workspace(ui);
        });
        let elapsed_us = started.elapsed().as_secs_f64() * 1_000_000.0;
        FrameSample {
            stage,
            index,
            elapsed_us,
            revision: shell
                .runtime
                .as_ref()
                .and_then(RuntimeController::displayed_revision)
                .unwrap_or(0),
            nodes: shell
                .runtime
                .as_ref()
                .and_then(RuntimeController::snapshot)
                .map_or(0, |snapshot| snapshot.len()),
            ranked_rows: shell.list_cache.rows.len(),
            shapes: output.shapes.len(),
        }
    }

    #[test]
    #[ignore = "manual release A/B benchmark; see tools/diskpie-bench/scripts/ui-benchmark.ps1"]
    fn frames_csv() {
        let entries = std::env::var("DISKPIE_UI_BENCH_ENTRIES").unwrap().parse::<usize>().unwrap();
        let variant = std::env::var("DISKPIE_UI_BENCH_VARIANT").unwrap();
        let run = std::env::var("DISKPIE_UI_BENCH_RUN").unwrap();
        let output = std::env::var_os("DISKPIE_UI_BENCH_OUTPUT").unwrap();
        let (mut shell, ctx) = headless_shell(None);
        settle(&mut shell, &ctx);
        for index in 0..20 {
            let _ = frame(&mut shell, &ctx, "empty_warmup", index, None);
        }
        shell
            .runtime
            .as_mut()
            .unwrap()
            .request_scan(
                Arc::new(FlatFs(entries)),
                vec![ScanRoot::new(
                    fixture_root(),
                    diskpie_core::VolumeKey::new(1),
                    diskpie_scan::StorageClass::LocalLowSeekPenalty,
                )],
                CancelToken::new(),
            )
            .expect("synthetic scan launches");
        // Poll publication without ranking/rendering any snapshot. This isolates
        // the first frame's UI work from the preceding asynchronous scan.
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            shell.logic(&ctx);
            let runtime = shell.runtime.as_ref().unwrap();
            if runtime.snapshot().is_some_and(|snapshot| snapshot.len() == entries + 1)
                && !runtime.needs_repaint()
            {
                break;
            }
            assert!(Instant::now() < deadline, "synthetic scan did not publish final frame");
            std::thread::sleep(Duration::from_millis(2));
        }
        let mut samples = Vec::with_capacity(802);
        samples.push(frame(&mut shell, &ctx, "snapshot_first", 0, None));
        for index in 0..200 {
            samples.push(frame(&mut shell, &ctx, "snapshot_transition", index, None));
            // Give asynchronous workers two seconds to settle. The sleep is
            // excluded from each frame duration; transition spikes are retained.
            std::thread::sleep(Duration::from_millis(10));
        }
        let expected_rows = if variant == "baseline" { entries.min(200) } else { entries };
        assert_eq!(shell.list_cache.rows.len(), expected_rows, "list did not settle");
        for index in 0..200 {
            samples.push(frame(&mut shell, &ctx, "warm", index, None));
        }
        let metric = if shell.metric == SizeBasis::Logical {
            SizeBasis::Allocated
        } else {
            SizeBasis::Logical
        };
        samples.push(frame(&mut shell, &ctx, "metric_first", 0, Some(metric)));
        for index in 0..200 {
            samples.push(frame(&mut shell, &ctx, "metric_transition", index, None));
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(shell.list_cache.rows.len(), expected_rows);
        assert!(!shell.runtime.as_ref().unwrap().needs_repaint(), "metric layout did not settle");
        for index in 0..200 {
            samples.push(frame(&mut shell, &ctx, "metric_warm", index, None));
        }
        let mut file = std::io::BufWriter::new(std::fs::File::create(output).unwrap());
        writeln!(file, "variant,run,entries,stage,frame,frame_us,viewport_width,viewport_height,revision,nodes,ranked_rows,shapes").unwrap();
        for sample in samples {
            writeln!(
                file,
                "{variant},{run},{entries},{},{},{:.3},1280,800,{},{},{},{}",
                sample.stage,
                sample.index,
                sample.elapsed_us,
                sample.revision,
                sample.nodes,
                sample.ranked_rows,
                sample.shapes
            )
            .unwrap();
        }
        file.flush().unwrap();
        shell.begin_shutdown();
    }
}
