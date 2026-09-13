//! Dependency-free benchmark driver; each sample runs in a fresh process.
use diskpie_app::session::SessionReducer;
use diskpie_core::{
    EntryKind, FileIdentity, MetricSource, NodeId, OwnMetrics, SizeMetric, VolumeKey,
    sunburst::{HiddenBranches, LayoutOptions, compute_layout},
};
use diskpie_scan::{
    CancelToken, DirectoryItem, FsEntry, FsError, FsErrorKind, FsOperation, ScanEvent, ScanFs,
    ScanGeneration, ScanRequest, ScanRoot, StorageClass, VisitControl, start_scan_with_cancel,
};
use std::{
    error::Error,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

#[cfg(windows)]
#[path = "../../../crates/diskpie-platform/src/windows/batch_prototype.rs"]
mod batch_prototype;

struct Synthetic {
    entries: usize,
    scenario: String,
}
impl ScanFs for Synthetic {
    fn visit_directory(
        &self,
        path: &Path,
        visitor: &mut dyn FnMut(DirectoryItem) -> VisitControl,
    ) -> Result<(), FsError> {
        let depth = path.components().count() - 1;
        let root = depth == 0;
        let wide = self.scenario == "wide";
        let wide_directories = self.entries / 10;
        let deep = self.scenario == "deep";
        let balanced = self.scenario == "balanced";
        if balanced && depth < 3 {
            for n in 0..16 {
                if visitor(DirectoryItem::Entry(FsEntry::new(
                    format!("d{n:02}"),
                    EntryKind::Directory,
                    OwnMetrics::ZERO_BY_POLICY,
                ))) == VisitControl::Stop
                {
                    return Ok(());
                }
            }
            return Ok(());
        }
        if deep
            && depth < 128
            && visitor(DirectoryItem::Entry(FsEntry::new(
                "deep-directory",
                EntryKind::Directory,
                OwnMetrics::ZERO_BY_POLICY,
            ))) == VisitControl::Stop
        {
            return Ok(());
        }
        let (start, stride, count) = if deep {
            (depth, 129, self.entries.saturating_sub(depth).div_ceil(129))
        } else if balanced {
            let leaf = path.components().skip(1).fold(0usize, |number, component| {
                number * 16
                    + component
                        .as_os_str()
                        .to_str()
                        .and_then(|name| name.strip_prefix('d'))
                        .and_then(|n| n.parse::<usize>().ok())
                        .unwrap_or(0)
            });
            (leaf, 4096, self.entries.saturating_sub(leaf).div_ceil(4096))
        } else if wide && !root {
            (
                path.file_name()
                    .and_then(|name| name.to_str())
                    .and_then(|name| name.strip_prefix("entry-"))
                    .and_then(|n| n.parse::<usize>().ok())
                    .unwrap_or(0)
                    * 9,
                1,
                9,
            )
        } else if wide {
            (0, 1, wide_directories)
        } else {
            (0, 1, self.entries)
        };
        for offset in 0..count {
            let n = start + offset * stride;
            if self.scenario == "slow" && n % 512 == 0 {
                std::thread::sleep(Duration::from_millis(1));
            }
            if self.scenario == "omissions" && n % 100 == 0 {
                if visitor(DirectoryItem::Omission(
                    FsError::new(
                        path.join(format!("denied-{n}")),
                        FsOperation::ReadMetadata,
                        FsErrorKind::AccessDenied,
                    )
                    .into(),
                )) == VisitControl::Stop
                {
                    break;
                }
                continue;
            }
            let kind = if root && wide { EntryKind::Directory } else { EntryKind::File };
            let metrics = OwnMetrics::new(
                SizeMetric::known((n % 1000 + 1) as u64, MetricSource::PortableMetadata),
                SizeMetric::known(
                    ((n % 1000 + 4096) / 4096 * 4096) as u64,
                    MetricSource::FilesystemAllocation,
                ),
            );
            let mut entry = FsEntry::new(
                format!("entry-{n:08}"),
                kind,
                if kind == EntryKind::Directory { OwnMetrics::ZERO_BY_POLICY } else { metrics },
            );
            if kind == EntryKind::File {
                let identity = n as u128;
                entry = entry.with_file_identity(FileIdentity::new(VolumeKey::new(1), identity));
            }
            if visitor(DirectoryItem::Entry(entry)) == VisitControl::Stop {
                break;
            }
        }
        Ok(())
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = std::env::args().collect::<Vec<_>>();
    let arg = |name: &str, default: &str| {
        args.windows(2)
            .find(|pair| pair[0] == name)
            .map_or_else(|| default.to_owned(), |pair| pair[1].clone())
    };
    let scenario = arg("--scenario", "flat");
    let entries: usize = arg("--entries", "100000").parse()?;
    let variant = arg("--variant", "current");
    let run = arg("--run", "1");
    let token = CancelToken::new();
    let (provider, root): (Arc<dyn ScanFs>, ScanRoot) =
        if scenario == "real" || scenario == "batch" || scenario == "differential" {
            let path = PathBuf::from(arg("--root", ""));
            #[cfg(windows)]
            {
                let scan_root = diskpie_platform::scan_root_from_path(&path)?;
                if scenario == "differential" {
                    return differential(&path);
                }
                let provider: Arc<dyn ScanFs> = if scenario == "batch" {
                    Arc::new(batch_prototype::BatchPrototype)
                } else {
                    Arc::new(diskpie_platform::WindowsFileSystem::with_cancel_token(token.clone()))
                };
                (provider, scan_root)
            }
            #[cfg(not(windows))]
            {
                let _ = path;
                return Err("real benchmark requires Windows".into());
            }
        } else {
            (
                Arc::new(Synthetic { entries, scenario: scenario.clone() }),
                ScanRoot::new(
                    if scenario == "wide" {
                        "synthetic-parent-prefix-".repeat(8)
                    } else {
                        "synthetic".into()
                    },
                    VolumeKey::new(1),
                    StorageClass::LocalLowSeekPenalty,
                ),
            )
        };
    let started = Instant::now();
    let generation = ScanGeneration::new(1);
    let session = start_scan_with_cancel(
        Arc::clone(&provider),
        ScanRequest::new(generation, vec![root.clone()]),
        token,
    )?;
    let mut reducer = SessionReducer::new(generation);
    let mut first_result = None;
    let terminal = loop {
        let event = session.recv_timeout(Duration::from_secs(60))?;
        if matches!(event, ScanEvent::Batch(_)) && first_result.is_none() {
            first_result = Some(started.elapsed());
        }
        let terminal = if let ScanEvent::Terminal(ref terminal) = event {
            Some(terminal.clone())
        } else {
            None
        };
        reducer.apply_event(event)?;
        if let Some(terminal) = terminal {
            break terminal;
        }
    };
    session.join()?;
    let scan = started.elapsed();
    let materialize = Instant::now();
    let snapshot = reducer.materialize_snapshot()?;
    let snapshot_time = materialize.elapsed();
    let layout_start = Instant::now();
    let layout =
        compute_layout(&snapshot, LayoutOptions::new(NodeId::from_raw(0)), &HiddenBranches::new())?;
    let layout_time = layout_start.elapsed();
    let cancel = CancelToken::new();
    let cancelled_session = start_scan_with_cancel(
        provider,
        ScanRequest::new(ScanGeneration::new(2), vec![root]),
        cancel.clone(),
    )?;
    // Cancel after work starts; exclude launch time from the latency metric.
    loop {
        if matches!(
            cancelled_session.recv_timeout(Duration::from_secs(60))?,
            ScanEvent::Batch(_) | ScanEvent::Terminal(_)
        ) {
            break;
        }
    }
    let cancelled_at = Instant::now();
    cancel.cancel();
    cancelled_session.join()?;
    let cancellation = cancelled_at.elapsed();
    let total = snapshot.nodes()[0].aggregate();
    println!(
        "variant,run,scenario,requested_entries,nodes,scan_reduce_ms,snapshot_ms,layout_ms,first_batch_ms,cancel_join_ms,peak_pending,sectors,node_record_bytes,logical_bytes,allocated_bytes"
    );
    println!(
        "{variant},{run},{scenario},{entries},{},{:.3},{:.3},{:.3},{:.3},{:.3},{},{},{},{},{}",
        snapshot.len(),
        scan.as_secs_f64() * 1000.0,
        snapshot_time.as_secs_f64() * 1000.0,
        layout_time.as_secs_f64() * 1000.0,
        first_result.unwrap_or(scan).as_secs_f64() * 1000.0,
        cancellation.as_secs_f64() * 1000.0,
        terminal.peak_pending_directories,
        layout.sectors().len(),
        std::mem::size_of::<diskpie_core::NodeRecord>(),
        total.logical().known_bytes(),
        total.allocated().known_bytes()
    );
    Ok(())
}

#[cfg(windows)]
fn differential(path: &Path) -> Result<(), Box<dyn Error>> {
    fn collect(provider: &dyn ScanFs, path: &Path) -> Result<Vec<FsEntry>, FsError> {
        let mut entries = Vec::new();
        let mut omission = None;
        provider.visit_directory(path, &mut |item| {
            match item {
                DirectoryItem::Entry(entry) => entries.push(entry),
                DirectoryItem::Omission(error) => omission = Some(error.error),
            };
            VisitControl::Continue
        })?;
        if let Some(error) = omission {
            return Err(error);
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }
    let conventional = collect(&diskpie_platform::WindowsFileSystem::new(), path)?;
    let batch = collect(&batch_prototype::BatchPrototype, path)?;
    if conventional.len() != batch.len() {
        return Err("entry count differs".into());
    }
    for (left, right) in conventional.iter().zip(&batch) {
        if left.name != right.name
            || left.kind != right.kind
            || left.own_metrics.logical().known_bytes() != right.own_metrics.logical().known_bytes()
            || left.own_metrics.allocated().known_bytes()
                != right.own_metrics.allocated().known_bytes()
            || left.file_identity != right.file_identity
        {
            return Err(format!(
                "metadata mismatch for {:?}: {:?} vs {:?}",
                left.name, left, right
            )
            .into());
        }
    }
    println!("differential_entries={},result=equal", conventional.len());
    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::{
        ffi::OsString,
        os::windows::ffi::OsStringExt,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn batch_matches_conventional_for_native_names_hard_links_and_directories() {
        let fixture = std::env::temp_dir().join(format!(
            "DiskPieBatchFixture-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        // create_dir grants ownership only of this newly created exact fixture.
        std::fs::create_dir(&fixture).expect("new unique fixture");
        std::fs::write(fixture.join("source😀資料"), vec![0x51; 8192]).unwrap();
        std::fs::hard_link(fixture.join("source😀資料"), fixture.join("alias")).unwrap();
        std::fs::write(
            fixture.join(OsString::from_wide(&[0xD800, b'-' as u16, b'x' as u16])),
            b"native",
        )
        .unwrap();
        std::fs::create_dir(fixture.join("directory")).unwrap();
        differential(&fixture).expect("native metadata parity");
        std::fs::remove_dir_all(&fixture).expect("delete only self-created fixture");
    }
}
