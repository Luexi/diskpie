//! Benchmark-only FileIdExtdDirectoryInfo adapter. Not exported by platform.
//! Deliberately rejects reparse/cloud boundaries instead of claiming parity
//! with the conventional provider's complete boundary policy.
use diskpie_core::{EntryKind, FileIdentity, MetricSource, OwnMetrics, SizeMetric, VolumeKey};
use diskpie_scan::{
    DirectoryItem, FsEntry, FsError, FsErrorKind, FsOperation, ScanFs, VisitControl,
};
use std::{
    ffi::OsString,
    fs::OpenOptions,
    io,
    mem::{offset_of, size_of},
    os::windows::{
        ffi::OsStringExt,
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
    path::Path,
};
use windows::Win32::{
    Foundation::HANDLE,
    Storage::FileSystem::{
        FILE_ID_EXTD_DIR_INFO, FILE_ID_INFO, FileIdExtdDirectoryInfo, FileIdInfo,
        GetFileInformationByHandleEx,
    },
};

pub struct BatchPrototype;

fn failure(path: &Path, code: i32) -> FsError {
    let kind = match code {
        2 | 3 => FsErrorKind::NotFound,
        5 => FsErrorKind::AccessDenied,
        1 | 50 | 87 => FsErrorKind::NotSupported,
        _ => FsErrorKind::Io,
    };
    FsError::new(path, FsOperation::EnumerateDirectory, kind).with_os_code(code)
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid directory information record")
}

fn parse(buffer: &[u8], volume: VolumeKey) -> io::Result<Vec<FsEntry>> {
    const NAME: usize = offset_of!(FILE_ID_EXTD_DIR_INFO, FileName);
    let mut entries = Vec::new();
    let mut offset = 0;
    loop {
        let record = buffer.get(offset..).ok_or_else(invalid)?;
        if record.len() < NAME {
            return Err(invalid());
        }
        let u32_at = |index| {
            u32::from_le_bytes(record[index..index + 4].try_into().expect("validated header"))
        };
        let i64_at = |index| {
            i64::from_le_bytes(record[index..index + 8].try_into().expect("validated header"))
        };
        let next = u32_at(offset_of!(FILE_ID_EXTD_DIR_INFO, NextEntryOffset)) as usize;
        let length = u32_at(offset_of!(FILE_ID_EXTD_DIR_INFO, FileNameLength)) as usize;
        if length == 0 || !length.is_multiple_of(2) {
            return Err(invalid());
        }
        let end = NAME.checked_add(length).ok_or_else(invalid)?;
        let bytes = record.get(NAME..end).ok_or_else(invalid)?;
        let units =
            bytes.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect::<Vec<_>>();
        let name = OsString::from_wide(&units);
        if name != "." && name != ".." {
            let attributes = u32_at(offset_of!(FILE_ID_EXTD_DIR_INFO, FileAttributes));
            // Reparse, offline, recall-on-open and recall-on-data-access.
            if attributes & (0x400 | 0x1000 | 0x40000 | 0x400000) != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "prototype boundary requires conventional provider",
                ));
            }
            let directory = attributes & 0x10 != 0;
            let kind = if directory { EntryKind::Directory } else { EntryKind::File };
            let logical = u64::try_from(i64_at(offset_of!(FILE_ID_EXTD_DIR_INFO, EndOfFile)))
                .map_err(|_| invalid())?;
            let allocated =
                u64::try_from(i64_at(offset_of!(FILE_ID_EXTD_DIR_INFO, AllocationSize)))
                    .map_err(|_| invalid())?;
            let metrics = if directory {
                OwnMetrics::ZERO_BY_POLICY
            } else {
                OwnMetrics::new(
                    SizeMetric::known(logical, MetricSource::FilesystemAllocation),
                    SizeMetric::known(allocated, MetricSource::FilesystemAllocation),
                )
            };
            let mut entry = FsEntry::new(name, kind, metrics);
            if !directory {
                let start = offset_of!(FILE_ID_EXTD_DIR_INFO, FileId);
                let id = u128::from_le_bytes(
                    record[start..start + 16].try_into().expect("validated identity"),
                );
                entry = entry.with_file_identity(FileIdentity::new(volume, id));
            }
            entries.push(entry);
        }
        if next == 0 {
            return Ok(entries);
        }
        if next < end || !next.is_multiple_of(8) || next >= record.len() {
            return Err(invalid());
        }
        offset = offset.checked_add(next).ok_or_else(invalid)?;
    }
}

impl ScanFs for BatchPrototype {
    fn visit_directory(
        &self,
        path: &Path,
        visitor: &mut dyn FnMut(DirectoryItem) -> VisitControl,
    ) -> Result<(), FsError> {
        // File owns the native handle immediately; no raw ownership escapes.
        let file = OpenOptions::new()
            .access_mode(0x1 | 0x80)
            .share_mode(7)
            .custom_flags(0x02000000 | 0x00200000)
            .open(path)
            .map_err(|error| failure(path, error.raw_os_error().unwrap_or(1)))?;
        let attributes = file
            .metadata()
            .map_err(|error| failure(path, error.raw_os_error().unwrap_or(1)))?
            .file_attributes();
        if attributes & (0x400 | 0x1000 | 0x40000 | 0x400000) != 0 {
            return Err(failure(path, 50));
        }
        let handle = HANDLE(file.as_raw_handle());
        let mut identity = FILE_ID_INFO::default();
        // SAFETY: a live no-follow File handle and correctly sized writable struct.
        unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileIdInfo,
                (&raw mut identity).cast(),
                size_of::<FILE_ID_INFO>() as u32,
            )
        }
        .map_err(|error| failure(path, error.code().0 & 0xffff))?;
        let volume = VolumeKey::new(u128::from(identity.VolumeSerialNumber));
        let mut storage = vec![0_u64; 8192];
        loop {
            storage.fill(0);
            // SAFETY: the Vec supplies 64 KiB of live, aligned writable storage;
            // the retained File handle owns the enumeration cursor.
            let result = unsafe {
                GetFileInformationByHandleEx(
                    handle,
                    FileIdExtdDirectoryInfo,
                    storage.as_mut_ptr().cast(),
                    (storage.len() * 8) as u32,
                )
            };
            if let Err(error) = result {
                let code = error.code().0 & 0xffff;
                if code == 18 {
                    return Ok(());
                }
                return Err(failure(path, code));
            }
            // SAFETY: all storage bytes are initialized, owned and remain live
            // for the immutable parser borrow. No typed record casts are used.
            let bytes = unsafe {
                std::slice::from_raw_parts(storage.as_ptr().cast::<u8>(), storage.len() * 8)
            };
            let entries = parse(bytes, volume).map_err(|error| {
                failure(path, if error.kind() == io::ErrorKind::Unsupported { 50 } else { 13 })
            })?;
            for entry in entries {
                if visitor(DirectoryItem::Entry(entry)) == VisitControl::Stop {
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_truncated_and_invalid_offsets() {
        assert!(parse(&[0; 8], VolumeKey::new(1)).is_err());
        assert!(parse(&[0; 128], VolumeKey::new(1)).is_err());
        let mut record = vec![0; 128];
        let len = offset_of!(FILE_ID_EXTD_DIR_INFO, FileNameLength);
        record[len..len + 4].copy_from_slice(&2u32.to_le_bytes());
        let name = offset_of!(FILE_ID_EXTD_DIR_INFO, FileName);
        record[name..name + 2].copy_from_slice(&(b'x' as u16).to_le_bytes());
        record[0..4].copy_from_slice(&7u32.to_le_bytes());
        assert!(parse(&record, VolumeKey::new(1)).is_err());
        record[0..4].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(parse(&record, VolumeKey::new(1)).unwrap()[0].name, "x");
    }
}
