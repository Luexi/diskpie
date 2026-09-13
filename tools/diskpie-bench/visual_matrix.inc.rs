// Native, opt-in QA instrument. Included only inside shell::tests.
#[cfg(windows)]
mod visual_matrix {
    use super::*;
    use diskpie_core::{EntryKind, FileIdentity, MetricSource, OwnMetrics, SizeMetric, VolumeKey};
    use diskpie_scan::{
        DirectoryItem, FsEntry, FsError, FsErrorKind, FsOperation, StorageClass, VisitControl,
    };
    use std::{
        io::Write,
        sync::mpsc::{self, SyncSender},
        thread,
    };
    use winit::platform::windows::EventLoopBuilderExtWindows;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum State {
        Complete,
        Scanning,
        Partial,
        Cancelled,
        Stale,
        Retained,
    }

    #[derive(Clone, Debug)]
    struct Case {
        width: u32,
        height: u32,
        dark: bool,
        spanish: bool,
        scale: u32,
        list: bool,
        state: State,
    }

    impl Case {
        fn name(&self) -> String {
            format!(
                "{:?}-{}x{}-{}-{}-p{}-{}",
                self.state,
                self.width,
                self.height,
                if self.dark { "dark" } else { "light" },
                if self.spanish { "es" } else { "en" },
                self.scale * 100,
                if self.list { "b" } else { "a" }
            )
            .to_lowercase()
        }
    }

    fn fixture_path() -> PathBuf {
        PathBuf::from(r"C:\DiskPie-Visual-Fixture")
    }

    struct VisualFs {
        root: PathBuf,
        token: CancelToken,
        state: State,
    }

    fn wait_for_cancel(token: &CancelToken) {
        while !token.is_cancelled() {
            thread::sleep(Duration::from_millis(5));
        }
    }

    impl ScanFs for VisualFs {
        fn visit_directory(
            &self,
            path: &Path,
            visitor: &mut dyn FnMut(DirectoryItem) -> VisitControl,
        ) -> Result<(), FsError> {
            // Initial publication is unconditional: an active scan normally
            // becomes Partial { settled: false } before the native frame can
            // capture it. Hold some subdirectories, allowing useful results
            // from the completed siblings to remain visible during scanning.
            if self.state == State::Scanning && path.file_name() == Some("Actual".as_ref()) {
                wait_for_cancel(&self.token);
                return Ok(());
            }
            let root = path == self.root;
            if root {
                for name in ["Documentos", "Juegos", "Fotos", "Vídeos", "Trabajo"] {
                    if visitor(DirectoryItem::Entry(FsEntry::new(
                        name,
                        EntryKind::Directory,
                        OwnMetrics::ZERO_BY_POLICY,
                    ))) == VisitControl::Stop
                    {
                        return Ok(());
                    }
                }
            } else if path.parent() == Some(self.root.as_path()) {
                for name in ["Archivo", "Actual"] {
                    if visitor(DirectoryItem::Entry(FsEntry::new(
                        name,
                        EntryKind::Directory,
                        OwnMetrics::ZERO_BY_POLICY,
                    ))) == VisitControl::Stop
                    {
                        return Ok(());
                    }
                }
            }
            let path_key = path
                .to_string_lossy()
                .bytes()
                .fold(0_u64, |hash, byte| hash.wrapping_mul(131).wrapping_add(u64::from(byte)));
            // A complete root batch also lets the cancellation fixture publish
            // meaningful partial data before its final enumeration is held.
            let count = if root { 1024 } else { 18 };
            for index in 0..count {
                if self.token.is_cancelled() {
                    return Ok(());
                }
                let logical =
                    if root { 16 } else { (path_key % 9 + 1) * 64 * 1024 * 1024 * (index % 4 + 1) };
                let metrics = OwnMetrics::new(
                    SizeMetric::known(logical, MetricSource::PortableMetadata),
                    SizeMetric::known(
                        logical.div_ceil(4096) * 4096,
                        MetricSource::FilesystemAllocation,
                    ),
                );
                let entry =
                    FsEntry::new(format!("archivo-{index:04}.dat"), EntryKind::File, metrics)
                        .with_file_identity(FileIdentity::new(
                            VolumeKey::new(1),
                            (u128::from(path_key) << 64) | u128::from(index),
                        ));
                if visitor(DirectoryItem::Entry(entry)) == VisitControl::Stop {
                    return Ok(());
                }
            }
            if root && self.state == State::Partial {
                let _ = visitor(DirectoryItem::Omission(
                    FsError::new(
                        path.join("Sin permiso"),
                        FsOperation::ReadMetadata,
                        FsErrorKind::AccessDenied,
                    )
                    .into(),
                ));
            }
            if root && self.state == State::Cancelled {
                wait_for_cancel(&self.token);
            }
            Ok(())
        }
    }

    struct HeldBranch(CancelToken);
    impl ScanFs for HeldBranch {
        fn visit_directory(
            &self,
            _path: &Path,
            _visitor: &mut dyn FnMut(DirectoryItem) -> VisitControl,
        ) -> Result<(), FsError> {
            wait_for_cancel(&self.0);
            Ok(())
        }
    }

    struct Capture {
        case: Case,
        image: Arc<egui::ColorImage>,
        actual_points: Vec2,
        pixels_per_point: f32,
        state: ScanUi,
        notice: String,
    }

    struct CaptureApp {
        shell: DiskPieShell,
        case: Case,
        sender: SyncSender<Capture>,
        result: Arc<Mutex<Result<bool, String>>>,
        token: CancelToken,
        started: Instant,
        frames: u32,
        ready_frames: u32,
        requested: bool,
        cancel_requested: bool,
        branch_started: Option<u32>,
    }

    impl CaptureApp {
        fn new(
            ctx: &egui::Context,
            case: Case,
            fonts: SystemFonts,
            sender: SyncSender<Capture>,
            result: Arc<Mutex<Result<bool, String>>>,
            exit: ExitHandoff,
            resolver: crate::resolver::ResolverClient,
        ) -> Self {
            let mut store = crate::storage::SettingsDocumentStore::from_bytes(None);
            let settings = SettingsSession::load(&mut store);
            let runtime =
                RuntimeController::new(runtime_config_from_settings(settings.settings())).unwrap();
            let services = UiServices {
                runtime,
                picker: None,
                resolver,
                diagnostics: DiagnosticSink::disconnected(),
                fonts,
                item_list: Some(ItemListService::new().expect("visual item worker starts")),
                support: None,
                diagnostic_counters: None,
            };
            let mut shell =
                DiskPieShell::with_context(ctx, None, false, false, settings, services, exit)
                    .unwrap();
            shell.theme_preference =
                if case.dark { ThemePreference::Dark } else { ThemePreference::Light };
            ctx.set_theme(shell.theme_preference);
            ctx.set_pixels_per_point(case.scale as f32);
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                case.width as f32,
                case.height as f32,
            )));
            shell.select_locale(if case.spanish {
                Locale::SpanishMexico
            } else {
                Locale::EnglishUnitedStates
            });
            shell.show_item_list = case.list;
            shell.item_list_width_points = 300;
            shell.roots_display = vec![fixture_path().to_string_lossy().into_owned()];
            shell.location = shell.roots_display.first().cloned();
            let token = CancelToken::new();
            let state = if matches!(case.state, State::Stale | State::Retained) {
                State::Complete
            } else {
                case.state
            };
            shell.try_launch(DeferredLaunch {
                fs: Arc::new(VisualFs { root: fixture_path(), token: token.clone(), state }),
                requested_paths: vec![fixture_path()],
                roots: vec![ScanRoot::new(
                    fixture_path(),
                    VolumeKey::new(1),
                    StorageClass::LocalLowSeekPenalty,
                )],
                cancel: token.clone(),
                displays: shell.roots_display.clone(),
                issues: Vec::new(),
                frames: 0,
                initial_omissions: 0,
            });
            Self {
                shell,
                case,
                sender,
                result,
                token,
                started: Instant::now(),
                frames: 0,
                ready_frames: 0,
                requested: false,
                cancel_requested: false,
                branch_started: None,
            }
        }

        fn prepare_state(&mut self) {
            if self.case.state == State::Cancelled
                && !self.cancel_requested
                && self.frames > 12
                && self.shell.runtime.as_ref().unwrap().snapshot().is_some()
            {
                self.shell.dispatch(ShellCommand::Cancel);
                self.cancel_requested = true;
            }
            if self.case.state == State::Stale {
                let runtime = self.shell.runtime.as_ref().unwrap();
                if self.branch_started.is_none()
                    && runtime.snapshot().is_some()
                    && !runtime.needs_repaint()
                    && self.frames > 12
                {
                    self.shell.set_visual_pending_refresh();
                    self.branch_started = Some(self.frames);
                }
                return;
            }
            if self.case.state != State::Retained {
                return;
            }
            let runtime = self.shell.runtime.as_mut().unwrap();
            if self.branch_started.is_none()
                && runtime.snapshot().is_some()
                && !runtime.needs_repaint()
                && self.frames > 12
            {
                let target = runtime
                    .snapshot()
                    .unwrap()
                    .children(runtime.navigation().unwrap().view_root())
                    .unwrap()
                    .find(|(_, node)| node.kind() == EntryKind::Directory)
                    .map(|(id, _)| id)
                    .unwrap();
                let intent = runtime.branch_rescan_intent(target).unwrap();
                let branch_root = intent.native_path().unwrap();
                self.token = CancelToken::new();
                runtime
                    .request_branch_scan(
                        intent,
                        Arc::new(HeldBranch(self.token.clone())),
                        ScanRoot::new(
                            branch_root,
                            VolumeKey::new(1),
                            StorageClass::LocalLowSeekPenalty,
                        ),
                        self.token.clone(),
                    )
                    .expect("held branch starts");
                self.shell.branch_running = true;
                self.branch_started = Some(self.frames);
            } else if self.branch_started.is_some_and(|start| self.frames > start + 12)
                && !self.cancel_requested
            {
                self.shell.dispatch(ShellCommand::Cancel);
                self.cancel_requested = true;
            }
        }

        fn ready(&self) -> bool {
            let runtime = self.shell.runtime.as_ref().unwrap();
            match self.case.state {
                State::Complete => {
                    self.shell.scan_state() == ScanUi::Complete && !runtime.needs_repaint()
                }
                State::Scanning => matches!(
                    self.shell.scan_state(),
                    ScanUi::Scanning | ScanUi::Partial { settled: false }
                ),
                State::Partial => {
                    self.shell.scan_state() == ScanUi::Partial { settled: true }
                        && !runtime.needs_repaint()
                }
                State::Cancelled => {
                    self.shell.scan_state() == ScanUi::CancelledWithResults
                        && !runtime.needs_repaint()
                }
                State::Stale => {
                    self.shell.notice == Notice::Message(MessageId::ResultsStale)
                        && !runtime.needs_repaint()
                }
                State::Retained => {
                    self.shell.notice == Notice::Message(MessageId::ResultsRetained)
                        && !runtime.needs_repaint()
                }
            }
        }
    }

    impl eframe::App for CaptureApp {
        fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
            let ctx = ui.ctx().clone();
            let screenshot = ctx.input(|input| {
                input.events.iter().find_map(|event| match event {
                    egui::Event::Screenshot { image, .. } => Some(Arc::clone(image)),
                    _ => None,
                })
            });
            if let Some(image) = screenshot {
                let capture = Capture {
                    case: self.case.clone(),
                    image,
                    actual_points: ctx.content_rect().size(),
                    pixels_per_point: ctx.pixels_per_point(),
                    state: self.shell.scan_state(),
                    notice: self.shell.notice_text().to_owned(),
                };
                let result = self
                    .sender
                    .try_send(capture)
                    .map(|()| true)
                    .map_err(|error| format!("capture writer unavailable: {error}"));
                *self.result.lock().unwrap() = result;
                self.token.cancel();
                self.shell.begin_shutdown();
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
            self.frames += 1;
            self.prepare_state();
            // Keep QA pointer/key input deterministic while preserving native
            // viewport close events so the user can stop the window normally.
            ctx.input_mut(|input| {
                input.pointer = Default::default();
                input.events.retain(|event| {
                    !matches!(
                        event,
                        egui::Event::PointerMoved(_)
                            | egui::Event::PointerButton { .. }
                            | egui::Event::Key { .. }
                            | egui::Event::Text(_)
                    )
                });
            });
            // This is the production eframe entry point, not a reconstructed UI.
            eframe::App::ui(&mut self.shell, ui, frame);
            self.ready_frames = if self.ready() { self.ready_frames + 1 } else { 0 };
            if !self.requested
                && self.ready_frames >= 12
                && self.started.elapsed() > Duration::from_millis(300)
            {
                self.requested = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(
                    self.case.name(),
                )));
            }
            if self.started.elapsed() > Duration::from_secs(30) && !self.shell.shutdown_requested {
                *self.result.lock().unwrap() = Err(format!(
                    "{} timed out in {:?}; runtime {:?}, shutdown {}, notice {:?}",
                    self.case.name(),
                    self.shell.scan_state(),
                    self.shell.runtime.as_ref().unwrap().state(),
                    self.shell.shutdown_requested,
                    self.shell.notice
                ));
                self.token.cancel();
                self.shell.begin_shutdown();
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            ctx.request_repaint_after(Duration::from_millis(16));
        }

        fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
            eframe::App::clear_color(&self.shell, visuals)
        }
    }

    impl Drop for CaptureApp {
        fn drop(&mut self) {
            self.token.cancel();
            self.shell.begin_shutdown();
        }
    }

    fn write_capture(
        directory: &Path,
        capture: Capture,
        manifest: &mut impl Write,
    ) -> std::io::Result<()> {
        let name = capture.case.name();
        let path = directory.join(format!("{name}.ppm"));
        let [width, height] = capture.image.size;
        let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
        writeln!(file, "P6\n{width} {height}\n255")?;
        for pixel in &capture.image.pixels {
            file.write_all(&pixel.to_array()[..3])?;
        }
        file.flush()?;
        let quote = |text: &str| format!("\"{}\"", text.replace('"', "\"\""));
        writeln!(
            manifest,
            "{},{},{},{},{},{},{},{},{},{:.3},{:.3},{:.3},{},{}",
            name,
            capture.case.width,
            capture.case.height,
            capture.case.scale,
            if capture.case.dark { "dark" } else { "light" },
            if capture.case.spanish { "es" } else { "en" },
            if capture.case.list { "b" } else { "a" },
            width,
            height,
            capture.actual_points.x,
            capture.actual_points.y,
            capture.pixels_per_point,
            quote(&format!("{:?}", capture.state)),
            quote(&capture.notice)
        )?;
        manifest.flush()
    }

    #[test]
    #[ignore = "opens native QA windows; set DISKPIE_VISUAL_OUTPUT, optional DISKPIE_VISUAL_FILTER"]
    fn native_screenshot_matrix() {
        let directory = PathBuf::from(
            std::env::var_os("DISKPIE_VISUAL_OUTPUT").expect("explicit output directory"),
        );
        std::fs::create_dir_all(&directory).unwrap();
        let filter = std::env::var("DISKPIE_VISUAL_FILTER").unwrap_or_default();
        let mut cases = Vec::new();
        for (width, height) in [(1280, 850), (800, 600), (1920, 1080)] {
            for dark in [false, true] {
                for spanish in [false, true] {
                    for scale in [1, 2] {
                        for list in [false, true] {
                            cases.push(Case {
                                width,
                                height,
                                dark,
                                spanish,
                                scale,
                                list,
                                state: State::Complete,
                            });
                        }
                    }
                }
            }
        }
        // Explicitly exercise the responsive mode switch below 800 points,
        // preserving the original 48-case size/theme/locale/scale matrix.
        for list in [false, true] {
            cases.push(Case {
                width: 700,
                height: 600,
                dark: false,
                spanish: true,
                scale: 1,
                list,
                state: State::Complete,
            });
        }
        for state in
            [State::Scanning, State::Partial, State::Cancelled, State::Stale, State::Retained]
        {
            for list in [false, true] {
                cases.push(Case {
                    width: 1280,
                    height: 850,
                    dark: false,
                    spanish: true,
                    scale: 1,
                    list,
                    state,
                });
            }
        }
        cases.retain(|case| case.name().contains(&filter));
        assert!(!cases.is_empty(), "filter matched no visual cases");
        let expected = cases.len();
        // Match production font loading before any native window exists.
        // Record availability so evidence cannot confuse embedded fallback
        // rendering with Segoe UI on a host where that font is installed.
        let fonts = crate::fonts::load_windows_fallbacks();
        let mut font_evidence = format!(
            "segoe_ui_loaded={}\nskipped_for_budget={}\n",
            fonts.loaded.iter().any(|font| font.name == "system-segoeui.ttf-0"),
            fonts.skipped_for_budget
        );
        for font in &fonts.loaded {
            font_evidence.push_str(&format!(
                "{};face={};bytes={}\n",
                font.name,
                font.face_index,
                font.bytes.len()
            ));
        }
        std::fs::write(directory.join("fonts.txt"), font_evidence).unwrap();
        let (sender, receiver) = mpsc::sync_channel::<Capture>(2);
        let writer = thread::Builder::new().name("visual-capture-writer".into()).spawn(move || -> std::io::Result<usize> {
            let mut manifest = std::io::BufWriter::new(std::fs::File::create(directory.join("manifest.csv"))?);
            writeln!(manifest, "case,requested_width_points,requested_height_points,requested_pixels_per_point,theme,locale,mode,actual_width_pixels,actual_height_pixels,actual_width_points,actual_height_points,actual_pixels_per_point,observed_state,notice")?;
            let mut count = 0;
            for capture in receiver { write_capture(&directory, capture, &mut manifest)?; count += 1; }
            Ok(count)
        }).unwrap();
        for case in cases {
            let result = Arc::new(Mutex::new(Ok(false)));
            let result_for_app = Arc::clone(&result);
            let exit = ExitHandoff::default();
            let exit_for_app = exit.clone();
            let sender = sender.clone();
            let fonts = fonts.clone();
            let (resolver, resolver_worker) = crate::resolver::start(RejectingBackend).unwrap();
            let title = format!("DiskPie QA — {}", case.name());
            let native = eframe::NativeOptions {
                viewport: egui::ViewportBuilder::default()
                    .with_inner_size([case.width as f32, case.height as f32])
                    .with_position([20.0, 20.0])
                    .with_clamp_size_to_monitor_size(false)
                    .with_title(&title),
                renderer: eframe::Renderer::Glow,
                persist_window: false,
                event_loop_builder: Some(Box::new(|builder| {
                    builder.with_any_thread(true);
                })),
                ..Default::default()
            };
            eframe::run_native(
                &title,
                native,
                Box::new(move |creation| {
                    Ok(Box::new(CaptureApp::new(
                        &creation.egui_ctx,
                        case,
                        fonts,
                        sender,
                        result_for_app,
                        exit_for_app,
                        resolver,
                    )))
                }),
            )
            .expect("native screenshot app runs");
            if let Some(mut handoff) = exit.take() {
                if let Some(runtime) = handoff.runtime.as_mut() {
                    runtime.request_shutdown();
                    let deadline = Instant::now() + Duration::from_secs(2);
                    while !runtime.is_shutdown_complete() && Instant::now() < deadline {
                        let _ = runtime.tick(|| handoff.clock_origin.elapsed());
                        thread::sleep(Duration::from_millis(2));
                    }
                    assert!(
                        runtime.is_shutdown_complete(),
                        "visual runtime did not finish shutdown"
                    );
                }
                if let Some(items) = handoff.item_list.take() {
                    assert_eq!(
                        items.finish(Duration::from_secs(1)),
                        diskpie_app::item_list_service::ItemListFinish::Joined
                    );
                }
            }
            assert!(resolver_worker.finish(Duration::from_secs(1)), "visual resolver did not join");
            assert_eq!(
                *result.lock().unwrap(),
                Ok(true),
                "native capture failed or window closed early"
            );
        }
        drop(sender);
        assert_eq!(writer.join().expect("writer thread").expect("write captures"), expected);
    }
}
