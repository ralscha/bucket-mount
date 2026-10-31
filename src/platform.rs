//! Native Windows Cloud Files integration. Remote objects have real NTFS
//! placeholders; opening a file hydrates that file in bounded, page-aligned
//! chunks. Other files remain online-only.
//!
//! All destructive placeholder mutations hold an exclusive, referenced CFAPI
//! oplock. An open editor makes those operations fail for a later retry.

use crate::namespace::{Namespace, OperationGuard};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    ffi::OsStr,
    fs::{File, OpenOptions},
    mem::{offset_of, size_of},
    os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle},
    path::Path,
    ptr::{null, null_mut},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use windows_sys::Win32::{
    Foundation::{ERROR_FILE_NOT_FOUND, GetLastError, HANDLE, STATUS_CLOUD_FILE_UNSUCCESSFUL},
    Storage::{CloudFilters::*, FileSystem::*},
    System::{
        IO::DeviceIoControl,
        Ioctl::{FSCTL_READ_FILE_USN_DATA, READ_FILE_USN_DATA, USN_RECORD_V2},
        Threading::GetCurrentProcessId,
    },
};

const TRANSFER_CHUNK: usize = 1024 * 1024;
const MAX_TRANSFERS: usize = 4;
const PAGE_SIZE: u64 = 4096;
const PROVIDER_NAME: &str = "Bucket Mount";
const ROOT_IDENTITY: &[u8] = b"bucket-mount:cfapi:v1";

/// Fetch implementations must apply the supplied ETag as an S3 If-Match
/// precondition. Returning bytes from a newer revision would corrupt a file.
pub trait Hydrator: Send + Sync + 'static {
    fn fetch(&self, key: &str, etag: &str, offset: u64, length: usize) -> Result<Vec<u8>>;
    fn accessed(&self, key: &str);
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileIdentity {
    pub key: String,
    pub etag: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FileState {
    pub placeholder: bool,
    pub in_sync: bool,
    pub pinned: bool,
    pub hydrated_bytes: u64,
}

type NamespaceOperations = HashMap<(i64, bool), Vec<(u64, OperationGuard)>>;

struct CallbackContext {
    hydrator: Arc<dyn Hydrator>,
    stopping: AtomicBool,
    transfers: Mutex<usize>,
    available: Condvar,
    cancellations: Mutex<HashMap<(i64, i64), Arc<AtomicBool>>>,
    namespace: Arc<Namespace>,
    namespace_operations: Mutex<NamespaceOperations>,
    next_operation: AtomicU64,
}

/// Keeps callback context alive until Windows has drained the connection.
pub struct CloudFiles {
    connection: CF_CONNECTION_KEY,
    context: Option<Box<CallbackContext>>,
}

impl CloudFiles {
    pub fn connect(root: &Path, hydrator: Arc<dyn Hydrator>) -> Result<Self> {
        ensure!(
            root.is_absolute(),
            "sync root must be an absolute local path"
        );
        std::fs::create_dir_all(root).context("create Cloud Files sync root")?;
        let root = wide_path(root)?;
        let name = wide(OsStr::new(PROVIDER_NAME))?;
        let version = wide(OsStr::new(env!("CARGO_PKG_VERSION")))?;

        // Re-register only our own roots. Never replace another provider's
        // registration (for example a directory beneath OneDrive).
        let mut previous = CF_SYNC_ROOT_PROVIDER_INFO::default();
        let existing = unsafe {
            CfGetSyncRootInfoByPath(
                root.as_ptr(),
                CF_SYNC_ROOT_INFO_PROVIDER,
                (&mut previous as *mut CF_SYNC_ROOT_PROVIDER_INFO).cast(),
                size_of::<CF_SYNC_ROOT_PROVIDER_INFO>() as u32,
                null_mut(),
            )
        };
        if existing >= 0 {
            let end = previous
                .ProviderName
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(256);
            ensure!(
                String::from_utf16_lossy(&previous.ProviderName[..end]) == PROVIDER_NAME,
                "the folder belongs to another cloud sync provider"
            );
        }
        let registration = CF_SYNC_REGISTRATION {
            StructSize: size_of::<CF_SYNC_REGISTRATION>() as u32,
            ProviderName: name.as_ptr(),
            ProviderVersion: version.as_ptr(),
            SyncRootIdentity: ROOT_IDENTITY.as_ptr().cast(),
            SyncRootIdentityLength: ROOT_IDENTITY.len() as u32,
            ..Default::default()
        };
        let policies = CF_SYNC_POLICIES {
            StructSize: size_of::<CF_SYNC_POLICIES>() as u32,
            Hydration: CF_HYDRATION_POLICY {
                // Full-on-open guarantees edits have a complete base revision
                // even if another device replaces the object during an edit.
                Primary: CF_HYDRATION_POLICY_FULL,
                Modifier: CF_HYDRATION_POLICY_MODIFIER_AUTO_DEHYDRATION_ALLOWED,
            },
            Population: CF_POPULATION_POLICY {
                Primary: CF_POPULATION_POLICY_ALWAYS_FULL,
                Modifier: CF_POPULATION_POLICY_MODIFIER_NONE,
            },
            InSync: CF_INSYNC_POLICY_TRACK_FILE_LAST_WRITE_TIME,
            HardLink: CF_HARDLINK_POLICY_NONE,
            PlaceholderManagement: CF_PLACEHOLDER_MANAGEMENT_POLICY_DEFAULT,
        };
        let flags = CF_REGISTER_FLAG_MARK_IN_SYNC_ON_ROOT
            | if existing >= 0 {
                CF_REGISTER_FLAG_UPDATE
            } else {
                0
            };
        check(
            unsafe { CfRegisterSyncRoot(root.as_ptr(), &registration, &policies, flags) },
            "register Cloud Files root (requires local NTFS and Windows 10 1709 or later)",
        )?;

        let context = Box::new(CallbackContext {
            hydrator,
            stopping: AtomicBool::new(false),
            transfers: Mutex::new(0),
            available: Condvar::new(),
            cancellations: Mutex::new(HashMap::new()),
            namespace: Arc::new(Namespace::new()),
            namespace_operations: Mutex::new(HashMap::new()),
            next_operation: AtomicU64::new(1),
        });
        let callbacks = [
            CF_CALLBACK_REGISTRATION {
                Type: CF_CALLBACK_TYPE_FETCH_DATA,
                Callback: Some(fetch_callback),
            },
            CF_CALLBACK_REGISTRATION {
                Type: CF_CALLBACK_TYPE_CANCEL_FETCH_DATA,
                Callback: Some(cancel_callback),
            },
            CF_CALLBACK_REGISTRATION {
                Type: CF_CALLBACK_TYPE_NOTIFY_FILE_OPEN_COMPLETION,
                Callback: Some(open_callback),
            },
            CF_CALLBACK_REGISTRATION {
                Type: CF_CALLBACK_TYPE_NOTIFY_RENAME,
                Callback: Some(rename_callback),
            },
            CF_CALLBACK_REGISTRATION {
                Type: CF_CALLBACK_TYPE_NOTIFY_RENAME_COMPLETION,
                Callback: Some(rename_completion_callback),
            },
            CF_CALLBACK_REGISTRATION {
                Type: CF_CALLBACK_TYPE_NOTIFY_DELETE,
                Callback: Some(delete_callback),
            },
            CF_CALLBACK_REGISTRATION {
                Type: CF_CALLBACK_TYPE_NOTIFY_DELETE_COMPLETION,
                Callback: Some(delete_completion_callback),
            },
            CF_CALLBACK_REGISTRATION {
                Type: CF_CALLBACK_TYPE_NONE,
                Callback: None,
            },
        ];
        let mut connection = 0;
        check(
            unsafe {
                CfConnectSyncRoot(
                    root.as_ptr(),
                    callbacks.as_ptr(),
                    (&*context as *const CallbackContext).cast(),
                    CF_CONNECT_FLAG_REQUIRE_PROCESS_INFO,
                    &mut connection,
                )
            },
            "connect Cloud Files provider",
        )?;
        Ok(Self {
            connection,
            context: Some(context),
        })
    }

    pub fn namespace(&self) -> Arc<Namespace> {
        self.context
            .as_ref()
            .expect("connected provider context")
            .namespace
            .clone()
    }
}

impl Drop for CloudFiles {
    fn drop(&mut self) {
        let Some(context) = self.context.take() else {
            return;
        };
        context.stopping.store(true, Ordering::Release);
        context.available.notify_all();
        // Windows finishes callbacks before this returns. Keep the registration
        // so offline placeholders retain their identity across application exits.
        let hr = unsafe { CfDisconnectSyncRoot(self.connection) };
        if hr < 0 {
            tracing::error!(
                hresult = format_args!("0x{:08X}", hr as u32),
                "Cloud Files disconnect failed"
            );
            // If disconnection fails, Windows may still invoke callbacks. Keep
            // their stopped context valid until process exit instead of a UAF.
            std::mem::forget(context);
        }
    }
}

struct TransferPermit<'a> {
    context: &'a CallbackContext,
    id: (i64, i64),
    cancelled: Arc<AtomicBool>,
}

impl<'a> TransferPermit<'a> {
    fn acquire(context: &'a CallbackContext, id: (i64, i64)) -> Result<Self> {
        let cancelled = Arc::new(AtomicBool::new(false));
        context
            .cancellations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, cancelled.clone());
        let mut active = context.transfers.lock().unwrap_or_else(|e| e.into_inner());
        while *active >= MAX_TRANSFERS && !context.stopping.load(Ordering::Acquire) {
            active = context
                .available
                .wait(active)
                .unwrap_or_else(|e| e.into_inner());
        }
        if context.stopping.load(Ordering::Acquire) {
            context
                .cancellations
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            bail!("provider is stopping");
        }
        *active += 1;
        Ok(Self {
            context,
            id,
            cancelled,
        })
    }

    fn check(&self) -> Result<()> {
        ensure!(
            !self.context.stopping.load(Ordering::Acquire)
                && !self.cancelled.load(Ordering::Acquire),
            "hydration was cancelled"
        );
        Ok(())
    }
}

impl Drop for TransferPermit<'_> {
    fn drop(&mut self) {
        self.context
            .cancellations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
        *self
            .context
            .transfers
            .lock()
            .unwrap_or_else(|e| e.into_inner()) -= 1;
        self.context.available.notify_one();
    }
}

unsafe extern "system" fn fetch_callback(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    if info.is_null() || params.is_null() {
        return;
    }
    // All callback-owned pointers are used synchronously, before returning to Windows.
    let info = unsafe { &*info };
    let request = unsafe { (*params).Anonymous.FetchData };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
        ensure!(!info.CallbackContext.is_null(), "missing callback context");
        let context = unsafe { &*info.CallbackContext.cast::<CallbackContext>() };
        let identity = callback_identity(info)?;
        let permit = TransferPermit::acquire(context, (info.TransferKey, info.RequestKey))?;
        let (mut offset, end) = aligned_range(
            request.RequiredFileOffset,
            request.RequiredLength,
            info.FileSize,
        )?;
        context.hydrator.accessed(&identity.key);
        while offset < end {
            permit.check()?;
            let length = (end - offset).min(TRANSFER_CHUNK as u64) as usize;
            let data = context
                .hydrator
                .fetch(&identity.key, &identity.etag, offset, length)?;
            ensure!(
                data.len() == length,
                "hydration returned {} bytes, expected {length}",
                data.len()
            );
            permit.check()?;
            transfer(info, offset as i64, length as i64, data.as_ptr().cast(), 0)?;
            offset += length as u64;
        }
        Ok(())
    }));
    match result {
        Ok(Ok(())) => {}
        failure => {
            let detail = match failure {
                Ok(Err(error)) => format!("{error:#}"),
                _ => "hydration callback panicked".to_owned(),
            };
            tracing::warn!(error = %detail, "Cloud Files hydration failed");
            // Every failed request must be completed, otherwise Explorer can hang
            // waiting for the provider timeout. Cover the entire original range.
            let (offset, end) = aligned_range(
                request.RequiredFileOffset,
                request.RequiredLength,
                info.FileSize,
            )
            .unwrap_or((
                request.RequiredFileOffset.max(0) as u64,
                info.FileSize.max(0) as u64,
            ));
            let _ = transfer(
                info,
                offset as i64,
                end.saturating_sub(offset) as i64,
                null(),
                STATUS_CLOUD_FILE_UNSUCCESSFUL,
            );
        }
    }
}

unsafe extern "system" fn cancel_callback(
    info: *const CF_CALLBACK_INFO,
    _params: *const CF_CALLBACK_PARAMETERS,
) {
    let _ = std::panic::catch_unwind(|| {
        if info.is_null() {
            return;
        }
        let info = unsafe { &*info };
        if info.CallbackContext.is_null() {
            return;
        }
        let context = unsafe { &*info.CallbackContext.cast::<CallbackContext>() };
        if let Some(flag) = context
            .cancellations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(info.TransferKey, info.RequestKey))
        {
            flag.store(true, Ordering::Release);
        }
        context.available.notify_all();
    });
}

unsafe extern "system" fn open_callback(
    info: *const CF_CALLBACK_INFO,
    _params: *const CF_CALLBACK_PARAMETERS,
) {
    let _ = std::panic::catch_unwind(|| {
        if info.is_null() {
            return;
        }
        let info = unsafe { &*info };
        if info.CallbackContext.is_null() {
            return;
        }
        let context = unsafe { &*info.CallbackContext.cast::<CallbackContext>() };
        // Our metadata scan must not make every cached file appear recently used.
        if !info.ProcessInfo.is_null()
            && unsafe { (*info.ProcessInfo).ProcessId == GetCurrentProcessId() }
        {
            return;
        }
        if let Ok(identity) = callback_identity(info) {
            context.hydrator.accessed(&identity.key);
        }
    });
}

fn callback_identity(info: &CF_CALLBACK_INFO) -> Result<FileIdentity> {
    ensure!(
        !info.FileIdentity.is_null()
            && info.FileIdentityLength > 0
            && info.FileIdentityLength <= CF_PLACEHOLDER_MAX_FILE_IDENTITY_LENGTH,
        "invalid placeholder identity"
    );
    let bytes = unsafe {
        std::slice::from_raw_parts(
            info.FileIdentity.cast::<u8>(),
            info.FileIdentityLength as usize,
        )
    };
    serde_json::from_slice(bytes).context("decode placeholder identity")
}

unsafe extern "system" fn rename_callback(
    info: *const CF_CALLBACK_INFO,
    _: *const CF_CALLBACK_PARAMETERS,
) {
    namespace_before(info, true);
}

unsafe extern "system" fn delete_callback(
    info: *const CF_CALLBACK_INFO,
    _: *const CF_CALLBACK_PARAMETERS,
) {
    namespace_before(info, false);
}

unsafe extern "system" fn rename_completion_callback(
    info: *const CF_CALLBACK_INFO,
    _: *const CF_CALLBACK_PARAMETERS,
) {
    namespace_completed(info, true);
}

unsafe extern "system" fn delete_completion_callback(
    info: *const CF_CALLBACK_INFO,
    _: *const CF_CALLBACK_PARAMETERS,
) {
    namespace_completed(info, false);
}

fn namespace_before(info: *const CF_CALLBACK_INFO, rename: bool) {
    if info.is_null() {
        return;
    }
    let info = unsafe { &*info };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
        ensure!(
            !info.CallbackContext.is_null(),
            "missing namespace callback context"
        );
        let context = unsafe { &*info.CallbackContext.cast::<CallbackContext>() };
        ensure!(
            !context.stopping.load(Ordering::Acquire),
            "provider is stopping"
        );
        // This may wait for an S3 deletion authorized against a stable scan.
        // Windows has not yet performed the local rename/delete at this point.
        let operation = context.namespace.begin_operation();
        let id = context.next_operation.fetch_add(1, Ordering::Relaxed);
        let key = (info.FileId, rename);
        context
            .namespace_operations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key)
            .or_default()
            .push((id, operation));
        let acknowledged = acknowledge_namespace(info, rename, 0);
        if acknowledged.is_err() {
            let mut operations = context
                .namespace_operations
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(pending) = operations.get_mut(&key) {
                pending.retain(|(pending_id, _)| *pending_id != id);
                if pending.is_empty() {
                    operations.remove(&key);
                }
            }
        }
        acknowledged
    }));
    if !matches!(result, Ok(Ok(()))) {
        tracing::warn!(rename, "could not authorize local namespace operation");
        let _ = acknowledge_namespace(info, rename, STATUS_CLOUD_FILE_UNSUCCESSFUL);
    }
}

fn namespace_completed(info: *const CF_CALLBACK_INFO, rename: bool) {
    let _ = std::panic::catch_unwind(|| {
        if info.is_null() {
            return;
        }
        let info = unsafe { &*info };
        if info.CallbackContext.is_null() {
            return;
        }
        let context = unsafe { &*info.CallbackContext.cast::<CallbackContext>() };
        let key = (info.FileId, rename);
        let mut operations = context
            .namespace_operations
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(pending) = operations.get_mut(&key) {
            // FileId remains stable across a rename. Completion callbacks have
            // their own request keys, so pair by file and operation kind.
            pending.pop();
            if pending.is_empty() {
                operations.remove(&key);
            }
        }
    });
}

fn acknowledge_namespace(info: &CF_CALLBACK_INFO, rename: bool, status: i32) -> Result<()> {
    let operation = CF_OPERATION_INFO {
        StructSize: size_of::<CF_OPERATION_INFO>() as u32,
        Type: if rename {
            CF_OPERATION_TYPE_ACK_RENAME
        } else {
            CF_OPERATION_TYPE_ACK_DELETE
        },
        ConnectionKey: info.ConnectionKey,
        TransferKey: info.TransferKey,
        CorrelationVector: info.CorrelationVector,
        RequestKey: info.RequestKey,
        ..Default::default()
    };
    let mut parameters = CF_OPERATION_PARAMETERS {
        ParamSize: (offset_of!(CF_OPERATION_PARAMETERS, Anonymous)
            + if rename {
                size_of::<CF_OPERATION_PARAMETERS_0_6>()
            } else {
                size_of::<CF_OPERATION_PARAMETERS_0_7>()
            }) as u32,
        Anonymous: if rename {
            CF_OPERATION_PARAMETERS_0 {
                AckRename: CF_OPERATION_PARAMETERS_0_6 {
                    Flags: CF_OPERATION_ACK_RENAME_FLAG_NONE,
                    CompletionStatus: status,
                },
            }
        } else {
            CF_OPERATION_PARAMETERS_0 {
                AckDelete: CF_OPERATION_PARAMETERS_0_7 {
                    Flags: CF_OPERATION_ACK_DELETE_FLAG_NONE,
                    CompletionStatus: status,
                },
            }
        },
    };
    check(
        unsafe { CfExecute(&operation, &mut parameters) },
        "acknowledge namespace operation",
    )
}

fn transfer(
    info: &CF_CALLBACK_INFO,
    offset: i64,
    length: i64,
    buffer: *const std::ffi::c_void,
    status: i32,
) -> Result<()> {
    let operation = CF_OPERATION_INFO {
        StructSize: size_of::<CF_OPERATION_INFO>() as u32,
        Type: CF_OPERATION_TYPE_TRANSFER_DATA,
        ConnectionKey: info.ConnectionKey,
        TransferKey: info.TransferKey,
        CorrelationVector: info.CorrelationVector,
        RequestKey: info.RequestKey,
        ..Default::default()
    };
    let mut parameters = CF_OPERATION_PARAMETERS {
        // CFAPI requires the active union member's size, not the full union size.
        ParamSize: (offset_of!(CF_OPERATION_PARAMETERS, Anonymous)
            + size_of::<CF_OPERATION_PARAMETERS_0_0>()) as u32,
        Anonymous: CF_OPERATION_PARAMETERS_0 {
            TransferData: CF_OPERATION_PARAMETERS_0_0 {
                Flags: CF_OPERATION_TRANSFER_DATA_FLAG_NONE,
                CompletionStatus: status,
                Buffer: buffer,
                Offset: offset,
                Length: length,
            },
        },
    };
    check(
        unsafe { CfExecute(&operation, &mut parameters) },
        "transfer Cloud Files range",
    )
}

fn aligned_range(offset: i64, length: i64, file_size: i64) -> Result<(u64, u64)> {
    ensure!(
        offset >= 0 && length > 0 && file_size >= 0,
        "invalid hydration range"
    );
    let offset = offset as u64;
    let requested_end = offset
        .checked_add(length as u64)
        .context("hydration range overflow")?;
    let start = offset / PAGE_SIZE * PAGE_SIZE;
    let end = requested_end
        .checked_add(PAGE_SIZE - 1)
        .context("hydration range overflow")?
        / PAGE_SIZE
        * PAGE_SIZE;
    ensure!(
        offset < file_size as u64,
        "hydration offset is beyond end of file"
    );
    Ok((start, end.min(file_size as u64)))
}

fn encoded_identity(key: &str, etag: &str) -> Result<Vec<u8>> {
    ensure!(
        !key.is_empty() && !etag.is_empty(),
        "key and ETag must not be empty"
    );
    let bytes = serde_json::to_vec(&FileIdentity {
        key: key.to_owned(),
        etag: etag.to_owned(),
    })?;
    ensure!(
        bytes.len() <= CF_PLACEHOLDER_MAX_FILE_IDENTITY_LENGTH as usize,
        "object identity exceeds Windows' 4 KiB limit"
    );
    Ok(bytes)
}

fn metadata(size: u64, modified_unix_ms: i64) -> Result<CF_FS_METADATA> {
    let timestamp = modified_unix_ms
        .checked_add(11_644_473_600_000)
        .and_then(|v| v.checked_mul(10_000))
        .context("timestamp outside Windows range")?;
    ensure!(timestamp > 0, "timestamp predates Windows FILETIME epoch");
    Ok(CF_FS_METADATA {
        FileSize: i64::try_from(size).context("file exceeds Windows size limit")?,
        BasicInfo: FILE_BASIC_INFO {
            CreationTime: timestamp,
            LastWriteTime: timestamp,
            FileAttributes: FILE_ATTRIBUTE_NORMAL,
            ..Default::default()
        },
    })
}

pub fn create_placeholder(
    path: &Path,
    key: &str,
    etag: &str,
    size: u64,
    modified_unix_ms: i64,
) -> Result<()> {
    let parent = path
        .parent()
        .context("placeholder has no parent directory")?;
    let filename = path.file_name().context("placeholder has no filename")?;
    std::fs::create_dir_all(parent)?;
    ensure_directory_placeholder(parent)?;
    let parent = wide_path(parent)?;
    let filename = wide(filename)?;
    let identity = encoded_identity(key, etag)?;
    let mut entry = CF_PLACEHOLDER_CREATE_INFO {
        RelativeFileName: filename.as_ptr(),
        FsMetadata: metadata(size, modified_unix_ms)?,
        FileIdentity: identity.as_ptr().cast(),
        FileIdentityLength: identity.len() as u32,
        Flags: CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC,
        ..Default::default()
    };
    let mut processed = 0;
    check(
        unsafe {
            CfCreatePlaceholders(
                parent.as_ptr(),
                &mut entry,
                1,
                CF_CREATE_FLAG_STOP_ON_ERROR,
                &mut processed,
            )
        },
        "create cloud placeholder",
    )?;
    ensure!(processed == 1, "Windows did not create the placeholder");
    check(entry.Result, "create cloud placeholder entry")
}

/// Convert existing directories up to this folder's registered sync root into
/// fully populated directory placeholders. This enables BEFORE rename/delete
/// callbacks when a user moves a parent containing online-only descendants.
pub fn ensure_directory_placeholder(path: &Path) -> Result<()> {
    let mut current = path;
    loop {
        let handle = attribute_handle(current)?;
        let mut file_info = BY_HANDLE_FILE_INFORMATION::default();
        bool_result(
            unsafe { GetFileInformationByHandle(handle.as_raw_handle(), &mut file_info) },
            "inspect cloud directory",
        )?;
        ensure!(
            file_info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0,
            "placeholder parent is not a directory"
        );
        let mut root = CF_SYNC_ROOT_BASIC_INFO::default();
        check(
            unsafe {
                CfGetSyncRootInfoByHandle(
                    handle.as_raw_handle(),
                    CF_SYNC_ROOT_INFO_BASIC,
                    (&mut root as *mut CF_SYNC_ROOT_BASIC_INFO).cast(),
                    size_of::<CF_SYNC_ROOT_BASIC_INFO>() as u32,
                    null_mut(),
                )
            },
            "find containing sync root",
        )?;
        let id = ((file_info.nFileIndexHigh as u64) << 32) | file_info.nFileIndexLow as u64;
        if id == root.SyncRootFileId as u64 {
            return Ok(());
        }
        let placeholder = inspect_handle(handle.as_raw_handle())?.0.placeholder;
        drop(handle);
        if !placeholder {
            let directory = ExclusiveFile::open(current, false)?;
            if !inspect_handle(directory.native)?.0.placeholder {
                check(
                    unsafe {
                        CfConvertToPlaceholder(
                            directory.native,
                            null(),
                            0,
                            CF_CONVERT_FLAG_MARK_IN_SYNC,
                            null_mut(),
                            null_mut(),
                        )
                    },
                    "convert cloud directory",
                )?;
            }
        }
        current = current
            .parent()
            .context("directory is not beneath its sync root")?;
    }
}

/// Atomically invalidates all cached data while changing the remote identity.
/// Dirty files or files currently opened by an editor are retained. Pinned
/// files keep their offline intent and rehydrate the new revision afterward.
pub fn update_placeholder(
    path: &Path,
    key: &str,
    etag: &str,
    size: u64,
    modified_unix_ms: i64,
) -> Result<()> {
    update_placeholder_guarded(path, key, etag, size, modified_unix_ms, None)
}

pub fn update_placeholder_if_unchanged(
    path: &Path,
    key: &str,
    etag: &str,
    size: u64,
    modified_unix_ms: i64,
    expected_version: u64,
) -> Result<()> {
    ensure!(
        expected_version > 0,
        "a nonzero USN is required for remote refresh"
    );
    update_placeholder_guarded(
        path,
        key,
        etag,
        size,
        modified_unix_ms,
        Some(expected_version),
    )
}

fn update_placeholder_guarded(
    path: &Path,
    key: &str,
    etag: &str,
    size: u64,
    modified_unix_ms: i64,
    expected_version: Option<u64>,
) -> Result<()> {
    let file = ExclusiveFile::open(path, false)?;
    if let Some(version) = expected_version {
        ensure!(
            version_handle(file.native)? == version,
            "file changed since scanning; deferring remote refresh"
        );
    }
    let state = inspect_handle(file.native)?.0;
    require_clean(&state)?;
    let identity = encoded_identity(key, etag)?;
    let mut meta = metadata(size, modified_unix_ms)?;
    // Preserve the original local creation date and attributes on remote edits.
    meta.BasicInfo.CreationTime = 0;
    meta.BasicInfo.FileAttributes = 0;
    let mut restore_pin = RestorePin::temporarily_unpin(&file, state.pinned)?;
    let update = check(
        unsafe {
            CfUpdatePlaceholder(
                file.native,
                &meta,
                identity.as_ptr().cast(),
                identity.len() as u32,
                null(),
                0,
                CF_UPDATE_FLAG_VERIFY_IN_SYNC
                    | CF_UPDATE_FLAG_MARK_IN_SYNC
                    | CF_UPDATE_FLAG_DEHYDRATE,
                null_mut(),
                null_mut(),
            )
        },
        "update remote placeholder",
    );
    let pin_result = restore_pin.restore();
    drop(restore_pin);
    drop(file);
    update?;
    pin_result?;
    if state.pinned {
        // Release the exclusive lock before hydration invokes callbacks. The
        // pinned flag prevents cache eviction while new bytes are fetched.
        hydrate(path).context("refresh pinned file contents")?;
    }
    Ok(())
}

/// Callers must ensure the bytes were durably uploaded before marking them clean.
/// Upload code should use `mark_synced_if_unchanged` to detect concurrent edits.
pub fn mark_synced(path: &Path, key: &str, etag: &str) -> Result<()> {
    let file = ExclusiveFile::open(path, false)?;
    mark_synced_handle(&file, key, etag)
}

pub fn mark_synced_if_unchanged(
    path: &Path,
    key: &str,
    etag: &str,
    expected_version: u64,
) -> Result<u64> {
    ensure!(
        expected_version > 0,
        "a nonzero USN is required to commit an upload"
    );
    let file = ExclusiveFile::open(path, false)?;
    ensure!(
        version_handle(file.native)? == expected_version,
        "file changed while uploading; retaining local changes"
    );
    mark_synced_handle(&file, key, etag)?;
    version_handle(file.native)
}

fn mark_synced_handle(file: &ExclusiveFile, key: &str, etag: &str) -> Result<()> {
    let identity = encoded_identity(key, etag)?;
    if inspect_handle(file.native)?.0.placeholder {
        // We already verified the journal USN under the exclusive lock. CFAPI's
        // update USN can include pending close bookkeeping not present in an
        // FSCTL_READ_FILE_USN_DATA snapshot; request its current output value.
        let mut usn = 0;
        check(
            unsafe {
                CfUpdatePlaceholder(
                    file.native,
                    null(),
                    identity.as_ptr().cast(),
                    identity.len() as u32,
                    null(),
                    0,
                    CF_UPDATE_FLAG_CLEAR_IN_SYNC,
                    &mut usn,
                    null_mut(),
                )
            },
            "update uploaded placeholder identity",
        )?;
        // Store the acknowledged identity before clearing dirty state. A crash
        // between these calls leaves a dirty file with the correct remote base.
        // The exclusive reference covers both calls and the preceding USN check.
        check(
            unsafe {
                CfSetInSyncState(
                    file.native,
                    CF_IN_SYNC_STATE_IN_SYNC,
                    CF_SET_IN_SYNC_FLAG_NONE,
                    &mut usn,
                )
            },
            "commit uploaded placeholder",
        )
    } else {
        check(
            unsafe {
                CfConvertToPlaceholder(
                    file.native,
                    identity.as_ptr().cast(),
                    identity.len() as u32,
                    CF_CONVERT_FLAG_MARK_IN_SYNC,
                    null_mut(),
                    null_mut(),
                )
            },
            "convert uploaded file to placeholder",
        )
    }
}

/// Hydrate before taking a stable upload snapshot. Closing this handle before
/// the snapshot's deny-write/delete handle is opened lets Windows commit the
/// hydration metadata and its USN bookkeeping independently of the snapshot.
pub fn hydrate(path: &Path) -> Result<()> {
    let handle = attribute_handle(path)?;
    let result = check(
        unsafe {
            CfHydratePlaceholder(
                handle.as_raw_handle(),
                0,
                -1,
                CF_HYDRATE_FLAG_NONE,
                null_mut(),
            )
        },
        "hydrate cloud file",
    );
    drop(handle);
    result
}

pub fn dehydrate(path: &Path) -> Result<()> {
    let file = ExclusiveFile::open(path, false)?;
    let state = inspect_handle(file.native)?.0;
    require_clean(&state)?;
    ensure!(!state.pinned, "file is pinned for offline use");
    check(
        unsafe {
            CfDehydratePlaceholder(file.native, 0, -1, CF_DEHYDRATE_FLAG_BACKGROUND, null_mut())
        },
        "evict cached cloud data",
    )
}

pub fn inspect(path: &Path) -> Result<FileState> {
    let file = attribute_handle(path)?;
    Ok(inspect_handle(file.as_raw_handle())?.0)
}

/// Read the placeholder state and the persisted identity through one handle.
/// A scan inspects every file it lists, so sharing one placeholder query with
/// the state read halves the metadata round trips per file. The persisted
/// identity also permits recovery if a crash occurs after a Cloud Files
/// operation but before committing the matching database record.
pub fn inspect_entry(path: &Path) -> Result<(FileState, Option<FileIdentity>)> {
    let file = attribute_handle(path)?;
    let (state, info) = inspect_handle(file.as_raw_handle())?;
    let identity = info.as_deref().map(identity_from_info).transpose()?;
    Ok((state, identity))
}

/// Inspect one file through a single handle, retaining both USN checks so a
/// concurrent edit cannot combine metadata from different versions of a file.
pub fn scan_entry(
    path: &Path,
) -> Result<(std::fs::Metadata, FileState, Option<FileIdentity>, u64)> {
    let file = attribute_handle(path)?;
    let handle = file.as_raw_handle();
    let version = version_handle(handle)?;
    let metadata = file.metadata()?;
    let (state, info) = inspect_handle(handle)?;
    let identity = info.as_deref().map(identity_from_info).transpose()?;
    ensure!(
        version_handle(handle)? == version,
        "file changed while inspecting; retrying scan"
    );
    Ok((metadata, state, identity, version))
}

fn identity_from_info(buffer: &[u64]) -> Result<FileIdentity> {
    let info = unsafe { &*buffer.as_ptr().cast::<CF_PLACEHOLDER_STANDARD_INFO>() };
    let start = offset_of!(CF_PLACEHOLDER_STANDARD_INFO, FileIdentity);
    let len = info.FileIdentityLength as usize;
    ensure!(
        len <= CF_PLACEHOLDER_MAX_FILE_IDENTITY_LENGTH as usize,
        "invalid placeholder identity length"
    );
    let bytes = unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>().add(start), len) };
    serde_json::from_slice(bytes).context("decode persisted placeholder identity")
}

pub fn remove_clean(path: &Path) -> Result<()> {
    let file = ExclusiveFile::open(path, true)?;
    require_clean(&inspect_handle(file.native)?.0)?;
    delete_handle(file.native)
}

pub fn remove_clean_if_unchanged(path: &Path, expected_version: u64) -> Result<()> {
    ensure!(
        expected_version > 0,
        "a nonzero USN is required for remote deletion"
    );
    let file = ExclusiveFile::open(path, true)?;
    ensure!(
        version_handle(file.native)? == expected_version,
        "file changed since scanning; deferring remote deletion"
    );
    require_clean(&inspect_handle(file.native)?.0)?;
    delete_handle(file.native)
}

/// Only use after durably saving a conflict copy. The USN check and deletion
/// occur under one exclusive lock, so a new edit is never silently deleted.
pub fn remove_if_unchanged(path: &Path, expected_version: u64) -> Result<()> {
    ensure!(
        expected_version > 0,
        "a nonzero USN is required to delete a saved conflict"
    );
    let file = ExclusiveFile::open(path, true)?;
    ensure!(
        version_handle(file.native)? == expected_version,
        "file changed since conflict backup"
    );
    delete_handle(file.native)
}

fn delete_handle(handle: HANDLE) -> Result<()> {
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    bool_result(
        unsafe {
            SetFileInformationByHandle(
                handle,
                FileDispositionInfo,
                (&disposition as *const FILE_DISPOSITION_INFO).cast(),
                size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        },
        "remove clean local placeholder",
    )
}

pub fn file_version(path: &Path) -> Result<u64> {
    file_version_from_file(&attribute_handle(path)?)
}

/// Persist this value in the local state database. A recreated sync directory
/// must be treated as a fresh mount, never as deletion of every remote object.
pub fn root_id(path: &Path) -> Result<String> {
    let file = attribute_handle(path)?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    bool_result(
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) },
        "read sync root identity",
    )?;
    ensure!(
        info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0,
        "sync root is not a directory"
    );
    Ok(format!(
        "{:08x}:{:08x}{:08x}",
        info.dwVolumeSerialNumber, info.nFileIndexHigh, info.nFileIndexLow
    ))
}

pub fn file_version_from_file(file: &File) -> Result<u64> {
    version_handle(file.as_raw_handle())
}

fn version_handle(handle: HANDLE) -> Result<u64> {
    let input = READ_FILE_USN_DATA {
        MinMajorVersion: 2,
        MaxMajorVersion: 2,
    };
    // u64 storage guarantees alignment for the USN_RECORD_V2 header.
    let mut output = [0_u64; 1024];
    let mut written = 0;
    bool_result(
        unsafe {
            DeviceIoControl(
                handle,
                FSCTL_READ_FILE_USN_DATA,
                (&input as *const READ_FILE_USN_DATA).cast(),
                size_of::<READ_FILE_USN_DATA>() as u32,
                output.as_mut_ptr().cast(),
                size_of_val(&output) as u32,
                &mut written,
                null_mut(),
            )
        },
        "read file change sequence (requires NTFS USN journal)",
    )?;
    ensure!(
        written as usize >= offset_of!(USN_RECORD_V2, FileName),
        "incomplete file change sequence"
    );
    let record = unsafe { &*output.as_ptr().cast::<USN_RECORD_V2>() };
    ensure!(
        record.MajorVersion == 2 && record.Usn > 0,
        "NTFS returned no usable file change sequence"
    );
    Ok(record.Usn as u64)
}

fn require_clean(state: &FileState) -> Result<()> {
    ensure!(
        state.placeholder && state.in_sync,
        "local file has unsynced changes"
    );
    Ok(())
}

fn attribute_handle(path: &Path) -> Result<File> {
    OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .with_context(|| format!("inspect {}", path.display()))
}

fn inspect_handle(handle: HANDLE) -> Result<(FileState, Option<Vec<u64>>)> {
    let mut tag = FILE_ATTRIBUTE_TAG_INFO::default();
    bool_result(
        unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileAttributeTagInfo,
                (&mut tag as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
                size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
            )
        },
        "read file attributes",
    )?;
    let state =
        unsafe { CfGetPlaceholderStateFromAttributeTag(tag.FileAttributes, tag.ReparseTag) };
    if state == CF_PLACEHOLDER_STATE_INVALID || state & CF_PLACEHOLDER_STATE_PLACEHOLDER == 0 {
        // Never convert/delete a symbolic link or an unrelated reparse point.
        ensure!(
            tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0,
            "file is an unsupported reparse point"
        );
        return Ok((FileState::default(), None));
    }
    let buffer = placeholder_info(handle)?;
    let info = unsafe { &*buffer.as_ptr().cast::<CF_PLACEHOLDER_STANDARD_INFO>() };
    Ok((
        FileState {
            placeholder: true,
            in_sync: info.InSyncState == CF_IN_SYNC_STATE_IN_SYNC && info.ModifiedDataSize == 0,
            pinned: info.PinState == CF_PIN_STATE_PINNED
                || tag.FileAttributes & FILE_ATTRIBUTE_PINNED != 0,
            hydrated_bytes: info.OnDiskDataSize.max(0) as u64,
        },
        Some(buffer),
    ))
}

fn placeholder_info(handle: HANDLE) -> Result<Vec<u64>> {
    let bytes = size_of::<CF_PLACEHOLDER_STANDARD_INFO>()
        + CF_PLACEHOLDER_MAX_FILE_IDENTITY_LENGTH as usize;
    let mut buffer = vec![0_u64; bytes.div_ceil(size_of::<u64>())];
    check(
        unsafe {
            CfGetPlaceholderInfo(
                handle,
                CF_PLACEHOLDER_INFO_STANDARD,
                buffer.as_mut_ptr().cast(),
                (buffer.len() * size_of::<u64>()) as u32,
                null_mut(),
            )
        },
        "query cloud placeholder state",
    )?;
    Ok(buffer)
}

/// Referencing the protected handle prevents CFAPI from closing it during an
/// oplock break between checking the file and performing the mutation.
struct ExclusiveFile {
    protected: HANDLE,
    native: HANDLE,
}

impl ExclusiveFile {
    fn open(path: &Path, delete: bool) -> Result<Self> {
        let path = wide_path(path)?;
        let mut protected = null_mut();
        check(
            unsafe {
                CfOpenFileWithOplock(
                    path.as_ptr(),
                    CF_OPEN_FILE_FLAG_EXCLUSIVE
                        | CF_OPEN_FILE_FLAG_WRITE_ACCESS
                        | if delete {
                            CF_OPEN_FILE_FLAG_DELETE_ACCESS
                        } else {
                            0
                        },
                    &mut protected,
                )
            },
            "lock cloud file (an application may still be using it)",
        )?;
        if !unsafe { CfReferenceProtectedHandle(protected) } {
            unsafe { CfCloseHandle(protected) };
            bail!("file lock was interrupted; retry later");
        }
        let native = unsafe { CfGetWin32HandleFromProtectedHandle(protected) };
        Ok(Self { protected, native })
    }
}

impl Drop for ExclusiveFile {
    fn drop(&mut self) {
        unsafe {
            CfReleaseProtectedHandle(self.protected);
            CfCloseHandle(self.protected);
        }
    }
}

struct RestorePin<'a> {
    file: &'a ExclusiveFile,
    active: bool,
}

impl<'a> RestorePin<'a> {
    fn temporarily_unpin(file: &'a ExclusiveFile, pinned: bool) -> Result<Self> {
        if pinned {
            check(
                unsafe {
                    CfSetPinState(
                        file.native,
                        CF_PIN_STATE_UNSPECIFIED,
                        CF_SET_PIN_FLAG_NONE,
                        null_mut(),
                    )
                },
                "prepare pinned file for remote update",
            )?;
        }
        Ok(Self {
            file,
            active: pinned,
        })
    }

    fn restore(&mut self) -> Result<()> {
        if self.active {
            check(
                unsafe {
                    CfSetPinState(
                        self.file.native,
                        CF_PIN_STATE_PINNED,
                        CF_SET_PIN_FLAG_NONE,
                        null_mut(),
                    )
                },
                "restore offline pin",
            )?;
            self.active = false;
        }
        Ok(())
    }
}

impl Drop for RestorePin<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            tracing::error!(%error, "could not restore offline pin after remote update");
        }
    }
}

/// A per-logon-session drive alias to the sync folder. No kernel driver or
/// elevation is required, and the exact alias is removed on graceful shutdown.
pub struct DriveAlias {
    device: Vec<u16>,
    target: Vec<u16>,
}

impl DriveAlias {
    pub fn create(letter: char, root: &Path) -> Result<Self> {
        ensure!(
            letter.is_ascii_alphabetic(),
            "drive letter must be A through Z"
        );
        let device = wide(OsStr::new(&format!("{}:", letter.to_ascii_uppercase())))?;
        let canonical = std::fs::canonicalize(root).context("resolve drive alias target")?;
        let text = canonical.to_string_lossy();
        let path = text.strip_prefix(r"\\?\").unwrap_or(&text);
        ensure!(
            !path.starts_with("UNC\\"),
            "drive aliases require a local sync folder"
        );
        let target = wide(OsStr::new(&format!(r"\??\{path}")))?;
        let mut existing = [0_u16; 32768];
        let found = unsafe {
            QueryDosDeviceW(
                device.as_ptr(),
                existing.as_mut_ptr(),
                existing.len() as u32,
            )
        };
        if found != 0 {
            let end = existing
                .iter()
                .position(|c| *c == 0)
                .context("invalid drive alias response")?;
            ensure!(
                existing[..end] == target[..target.len() - 1],
                "drive {}: is already in use",
                letter.to_ascii_uppercase()
            );
            // A crash leaves a logon-session alias behind. Reuse its exact
            // target instead of stacking a duplicate DOS-device definition.
            return Ok(Self { device, target });
        }
        ensure!(
            unsafe { GetLastError() } == ERROR_FILE_NOT_FOUND,
            "could not verify drive letter availability"
        );
        bool_result(
            unsafe { DefineDosDeviceW(DDD_RAW_TARGET_PATH, device.as_ptr(), target.as_ptr()) },
            "create drive alias",
        )?;
        Ok(Self { device, target })
    }
}

impl Drop for DriveAlias {
    fn drop(&mut self) {
        unsafe {
            DefineDosDeviceW(
                DDD_RAW_TARGET_PATH | DDD_REMOVE_DEFINITION | DDD_EXACT_MATCH_ON_REMOVE,
                self.device.as_ptr(),
                self.target.as_ptr(),
            );
        }
    }
}

fn wide(value: &OsStr) -> Result<Vec<u16>> {
    let mut encoded: Vec<u16> = value.encode_wide().collect();
    ensure!(
        !encoded.contains(&0),
        "Windows path contains a NUL character"
    );
    encoded.push(0);
    Ok(encoded)
}

/// Ordinary Win32 paths are limited to `MAX_PATH`. Object keys are not, so every
/// file path handed to Windows uses the verbatim form that the Rust standard
/// library also uses internally; otherwise deep keys fail as "not found".
fn wide_path(path: &Path) -> Result<Vec<u16>> {
    const BACKSLASH: u16 = b'\\' as u16;
    ensure!(path.is_absolute(), "Windows path must be absolute");
    let mut units: Vec<u16> = path.as_os_str().encode_wide().collect();
    // Verbatim paths bypass the normalization that would otherwise accept them.
    for unit in &mut units {
        if *unit == u16::from(b'/') {
            *unit = BACKSLASH;
        }
    }
    let mut encoded = Vec::with_capacity(units.len() + 8);
    if units.starts_with(&[BACKSLASH, BACKSLASH, u16::from(b'?'), BACKSLASH]) {
        encoded.extend_from_slice(&units);
    } else if units.starts_with(&[BACKSLASH, BACKSLASH]) {
        // A UNC share is addressed below the verbatim UNC root.
        encoded.extend("\\\\?\\UNC\\".encode_utf16());
        encoded.extend_from_slice(&units[2..]);
    } else {
        encoded.extend_from_slice(&[BACKSLASH, BACKSLASH, u16::from(b'?'), BACKSLASH]);
        encoded.extend_from_slice(&units);
    }
    ensure!(
        !encoded.contains(&0),
        "Windows path contains a NUL character"
    );
    encoded.push(0);
    Ok(encoded)
}

fn check(hr: i32, action: &str) -> Result<()> {
    if hr < 0 {
        Err(anyhow!("{action}: HRESULT 0x{:08X}", hr as u32))
    } else {
        Ok(())
    }
}

fn bool_result(ok: i32, action: &str) -> Result<()> {
    if ok == 0 {
        Err(std::io::Error::last_os_error()).context(action.to_owned())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbatim_paths_are_built_for_every_root() {
        let text = |path: &str| {
            let units = wide_path(Path::new(path)).unwrap();
            assert_eq!(*units.last().unwrap(), 0);
            String::from_utf16(&units[..units.len() - 1]).unwrap()
        };
        assert_eq!(text(r"C:\mount\file.txt"), r"\\?\C:\mount\file.txt");
        assert_eq!(text("C:/mount/file.txt"), r"\\?\C:\mount\file.txt");
        assert_eq!(
            text(r"\\server\share\file.txt"),
            r"\\?\UNC\server\share\file.txt"
        );
        assert_eq!(text(r"\\?\C:\mount\file.txt"), r"\\?\C:\mount\file.txt");
        assert!(wide_path(Path::new(r"mount\file.txt")).is_err());
    }

    #[test]
    fn hydration_ranges_are_aligned_without_reading_past_eof() {
        assert_eq!(aligned_range(1, 1, 20_000).unwrap(), (0, 4096));
        assert_eq!(aligned_range(8193, 1, 9000).unwrap(), (8192, 9000));
        assert_eq!(aligned_range(0, 16_384, 9000).unwrap(), (0, 9000));
        assert!(aligned_range(-1, 2, 9000).is_err());
        assert!(aligned_range(9000, 1, 9000).is_err());
        assert!(aligned_range(0, 0, 9000).is_err());
    }

    #[test]
    fn identities_preserve_object_keys_and_opaque_etags() {
        let bytes = encoded_identity("folder/日本語.txt", "\"opaque-etag-3\"").unwrap();
        let value: FileIdentity = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value.key, "folder/日本語.txt");
        assert_eq!(value.etag, "\"opaque-etag-3\"");
        assert!(encoded_identity(&"x".repeat(4096), "etag").is_err());
    }

    #[test]
    fn remote_mutations_require_clean_placeholders() {
        assert!(require_clean(&FileState::default()).is_err());
        assert!(
            require_clean(&FileState {
                placeholder: true,
                in_sync: false,
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            require_clean(&FileState {
                placeholder: true,
                in_sync: true,
                pinned: true,
                ..Default::default()
            })
            .is_ok()
        );
        assert!(
            require_clean(&FileState {
                placeholder: true,
                in_sync: true,
                ..Default::default()
            })
            .is_ok()
        );
    }

    /// Uses a disposable directory in the workspace, never an existing sync
    /// folder. Run on Windows with an NTFS volume and the Cloud Files filter:
    /// cargo test live_cloud_files_round_trip -- --ignored --nocapture
    #[test]
    #[ignore = "registers a disposable native Windows Cloud Files root"]
    fn live_cloud_files_round_trip() -> Result<()> {
        use std::io::Write;

        struct MemoryRemote {
            bytes: Mutex<Vec<u8>>,
            etag: Mutex<String>,
            requests: Mutex<Vec<(u64, usize)>>,
        }
        impl Hydrator for MemoryRemote {
            fn fetch(&self, key: &str, etag: &str, offset: u64, length: usize) -> Result<Vec<u8>> {
                ensure!(key == "example.bin", "unexpected key");
                ensure!(etag == *self.etag.lock().unwrap(), "wrong object revision");
                self.requests.lock().unwrap().push((offset, length));
                let bytes = self.bytes.lock().unwrap();
                Ok(bytes[offset as usize..offset as usize + length].to_vec())
            }
            fn accessed(&self, _key: &str) {}
        }
        struct Unregister(Vec<u16>);
        impl Drop for Unregister {
            fn drop(&mut self) {
                unsafe {
                    CfUnregisterSyncRoot(self.0.as_ptr());
                }
            }
        }

        let temp = tempfile::Builder::new()
            .prefix(".bucket-mount-cfapi-test-")
            .tempdir_in(std::env::current_dir()?)?;
        let _registration = Unregister(wide(temp.path().as_os_str())?);
        let contents: Vec<u8> = (0..TRANSFER_CHUNK * 2 + 123)
            .map(|i| (i % 251) as u8)
            .collect();
        let remote = Arc::new(MemoryRemote {
            bytes: Mutex::new(contents.clone()),
            etag: Mutex::new("first".into()),
            requests: Mutex::new(Vec::new()),
        });
        let connection = CloudFiles::connect(temp.path(), remote.clone())?;
        // The provider registers the verbatim form; an ordinary path must still
        // resolve so roots registered by earlier versions are recognized.
        let mut provider = CF_SYNC_ROOT_PROVIDER_INFO::default();
        ensure!(
            unsafe {
                CfGetSyncRootInfoByPath(
                    wide(temp.path().as_os_str())?.as_ptr(),
                    CF_SYNC_ROOT_INFO_PROVIDER,
                    (&mut provider as *mut CF_SYNC_ROOT_PROVIDER_INFO).cast(),
                    size_of::<CF_SYNC_ROOT_PROVIDER_INFO>() as u32,
                    null_mut(),
                )
            } >= 0,
            "registered sync root is not visible through its ordinary path"
        );
        let original_root_id = root_id(temp.path())?;
        let path = temp.path().join("example.bin");
        create_placeholder(
            &path,
            "example.bin",
            "first",
            contents.len() as u64,
            1_700_000_000_000,
        )?;
        let initial = inspect(&path)?;
        ensure!(
            initial.placeholder && initial.in_sync && initial.hydrated_bytes == 0,
            "new placeholder should be clean and dehydrated: {initial:?}"
        );
        ensure!(
            std::fs::read(&path)? == contents,
            "hydration changed file contents"
        );
        let requests = remote.requests.lock().unwrap();
        ensure!(
            requests.len() >= 3,
            "large hydration was not split into bounded chunks"
        );
        ensure!(
            requests.iter().all(|(_, length)| *length <= TRANSFER_CHUNK),
            "unbounded hydration chunk"
        );
        drop(requests);
        ensure!(
            inspect(&path)?.hydrated_bytes > 0,
            "hydration was not cached"
        );

        // A stale revision must fail the user I/O, not hang or return another
        // revision's bytes. A subsequent correct request remains recoverable.
        dehydrate(&path)?;
        *remote.etag.lock().unwrap() = "unavailable-old-revision".into();
        ensure!(
            std::fs::read(&path).is_err(),
            "hydration accepted the wrong remote revision"
        );
        *remote.etag.lock().unwrap() = "first".into();

        let (letter, alias) = ['Z', 'Y', 'X']
            .into_iter()
            .find_map(|letter| {
                DriveAlias::create(letter, temp.path())
                    .ok()
                    .map(|alias| (letter, alias))
            })
            .context("native drive test requires a free X:, Y:, or Z:")?;
        let aliased_file = std::path::PathBuf::from(format!("{letter}:\\example.bin"));
        ensure!(
            std::fs::read(&aliased_file)? == contents,
            "drive alias did not hydrate the correct bytes"
        );
        let device = alias.device.clone();
        drop(alias);
        let mut target = [0_u16; 32768];
        ensure!(
            unsafe { QueryDosDeviceW(device.as_ptr(), target.as_mut_ptr(), target.len() as u32) }
                == 0,
            "temporary drive alias was not removed"
        );

        // A separate process exercises the Windows BEFORE and completion
        // callbacks; provider-originated namespace changes can be suppressed.
        dehydrate(&path)?;
        let before_rename = file_version(&path)?;
        let namespace = connection.namespace();
        let generation = namespace.snapshot().context("namespace should be idle")?;
        let child = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "platform::tests::live_cloud_files_external_rename_helper",
                "--ignored",
                "--nocapture",
            ])
            .env("BUCKET_MOUNT_CFAPI_RENAME_SOURCE", &path)
            .status()?;
        ensure!(child.success(), "external rename test failed");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while namespace.snapshot().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        ensure!(
            namespace.snapshot().is_some_and(|after| after > generation),
            "rename callbacks did not invalidate and finish the namespace snapshot"
        );
        ensure!(
            root_id(temp.path())? == original_root_id,
            "rename changed root identity"
        );
        ensure!(
            update_placeholder_if_unchanged(
                &path,
                "example.bin",
                "first",
                contents.len() as u64,
                1_700_000_000_000,
                before_rename
            )
            .is_err(),
            "refresh accepted stale scan state"
        );
        ensure!(
            remove_clean_if_unchanged(&path, before_rename).is_err(),
            "deletion accepted stale scan state"
        );
        // Windows correctly clears in-sync on a rename, even when the file is
        // moved back. The fixture knows its bytes still match this remote ETag.
        // Hydration must finish and close before an upload snapshot records USN.
        hydrate(&path)?;
        let mut snapshot_source = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&path)?;
        std::io::copy(&mut snapshot_source, &mut std::io::sink())?;
        let snapshot_version = file_version_from_file(&snapshot_source)?;
        drop(snapshot_source);
        ensure!(
            file_version(&path)? == snapshot_version,
            "hydration changed the USN after the stable snapshot closed"
        );
        mark_synced_if_unchanged(&path, "example.bin", "first", snapshot_version)?;

        let directory = temp.path().join("nested");
        let nested_file = directory.join("child.bin");
        create_placeholder(
            &nested_file,
            "example.bin",
            "first",
            contents.len() as u64,
            1_700_000_000_000,
        )?;
        ensure!(
            inspect(&directory)?.placeholder,
            "parent directory is not protected by Cloud Files callbacks"
        );
        let before_directory_rename = namespace
            .snapshot()
            .context("directory namespace should be idle")?;
        let child = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "platform::tests::live_cloud_files_external_rename_helper",
                "--ignored",
                "--nocapture",
            ])
            .env("BUCKET_MOUNT_CFAPI_RENAME_SOURCE", &directory)
            .status()?;
        ensure!(child.success(), "external directory rename test failed");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while namespace.snapshot().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        ensure!(
            namespace
                .snapshot()
                .is_some_and(|after| after > before_directory_rename),
            "directory rename bypassed the namespace barrier"
        );
        ensure!(
            inspect(&nested_file)?.hydrated_bytes == 0,
            "directory rename unnecessarily downloaded a child"
        );

        // Updating the identity also invalidates all old cached bytes atomically.
        let replacement = vec![0x5a; contents.len() + 4096];
        *remote.bytes.lock().unwrap() = replacement.clone();
        *remote.etag.lock().unwrap() = "second".into();
        update_placeholder(
            &path,
            "example.bin",
            "second",
            replacement.len() as u64,
            1_700_000_001_000,
        )?;
        ensure!(
            inspect(&path)?.hydrated_bytes == 0,
            "remote update retained stale cached bytes"
        );
        ensure!(
            std::fs::read(&path)? == replacement,
            "updated remote revision did not hydrate"
        );
        dehydrate(&path)?;
        ensure!(
            inspect(&path)?.hydrated_bytes == 0,
            "cache eviction did not release file data"
        );

        // A user pin is honored even if the cache is over budget.
        {
            let handle = ExclusiveFile::open(&path, false)?;
            check(
                unsafe {
                    CfSetPinState(
                        handle.native,
                        CF_PIN_STATE_PINNED,
                        CF_SET_PIN_FLAG_NONE,
                        null_mut(),
                    )
                },
                "pin test file",
            )?;
        }
        ensure!(
            inspect(&path)?.pinned && dehydrate(&path).is_err(),
            "pinned file was evicted"
        );
        *remote.etag.lock().unwrap() = "pinned-revision".into();
        update_placeholder(
            &path,
            "example.bin",
            "pinned-revision",
            replacement.len() as u64,
            1_700_000_001_500,
        )?;
        ensure!(
            inspect(&path)?.pinned && inspect(&path)?.hydrated_bytes > 0,
            "remote update did not preserve and hydrate the offline pin"
        );
        ensure!(
            std::fs::read(&path)? == replacement,
            "pinned refresh returned stale bytes"
        );
        {
            let handle = ExclusiveFile::open(&path, false)?;
            check(
                unsafe {
                    CfSetPinState(
                        handle.native,
                        CF_PIN_STATE_UNSPECIFIED,
                        CF_SET_PIN_FLAG_NONE,
                        null_mut(),
                    )
                },
                "unpin test file",
            )?;
        }

        // Dirty bytes cannot be evicted or overwritten by a remote refresh.
        let mut writer = OpenOptions::new().write(true).open(&path)?;
        writer.write_all(b"local edit")?;
        writer.sync_all()?;
        drop(writer);
        ensure!(
            !inspect(&path)?.in_sync,
            "local edit did not clear in-sync state"
        );
        ensure!(dehydrate(&path).is_err(), "dirty file was evicted");
        ensure!(
            update_placeholder(
                &path,
                "example.bin",
                "second",
                replacement.len() as u64,
                1_700_000_002_000
            )
            .is_err(),
            "remote update discarded a dirty file"
        );

        let old_version = file_version(&path)?;
        let mut writer = OpenOptions::new().append(true).open(&path)?;
        writer.write_all(b" edit during upload")?;
        writer.sync_all()?;
        drop(writer);
        ensure!(
            mark_synced_if_unchanged(&path, "example.bin", "third", old_version).is_err(),
            "commit accepted a concurrent local edit"
        );
        let current_version = file_version(&path)?;
        mark_synced_if_unchanged(&path, "example.bin", "third", current_version)?;
        ensure!(inspect(&path)?.in_sync, "uploaded file remains dirty");
        ensure!(
            inspect_entry(&path)?.1.is_some_and(|id| id.etag == "third"),
            "uploaded ETag was not committed"
        );
        remove_clean(&path)?;
        ensure!(!path.exists(), "clean placeholder was not deleted");

        // Ordinary local files convert without losing their uploaded bytes.
        let ordinary = temp.path().join("ordinary.txt");
        std::fs::write(&ordinary, b"uploaded local file")?;
        ensure!(
            remove_clean(&ordinary).is_err(),
            "ordinary local file was deleted"
        );
        let version = file_version(&ordinary)?;
        mark_synced_if_unchanged(&ordinary, "ordinary.txt", "ordinary-etag", version)?;
        ensure!(inspect(&ordinary)?.in_sync, "normal file conversion failed");
        ensure!(
            std::fs::read(&ordinary)? == b"uploaded local file",
            "conversion changed bytes"
        );
        remove_clean(&ordinary)?;
        Ok(())
    }

    /// Object keys are not limited by `MAX_PATH`, so Cloud Files must be driven
    /// with verbatim paths. Uses a disposable directory, never an existing mount:
    /// cargo test live_cloud_files_long_paths -- --ignored --nocapture
    #[test]
    #[ignore = "registers a disposable native Windows Cloud Files root"]
    fn live_cloud_files_long_paths() -> Result<()> {
        struct MemoryRemote;
        impl Hydrator for MemoryRemote {
            fn fetch(&self, key: &str, etag: &str, _offset: u64, length: usize) -> Result<Vec<u8>> {
                ensure!(key.ends_with("/deep.bin"), "unexpected key");
                ensure!(etag == "long", "wrong object revision");
                Ok(vec![0x7f; length])
            }
            fn accessed(&self, _key: &str) {}
        }
        struct Unregister(Vec<u16>);
        impl Drop for Unregister {
            fn drop(&mut self) {
                unsafe {
                    CfUnregisterSyncRoot(self.0.as_ptr());
                }
            }
        }

        let temp = tempfile::Builder::new()
            .prefix(".bucket-mount-cfapi-long-path-")
            .tempdir_in(std::env::current_dir()?)?;
        let mut root = temp.path().to_path_buf();
        while root.as_os_str().len() < 260 {
            root = root.join("a-rather-long-directory-name-for-max-path-testing");
        }
        std::fs::create_dir_all(&root)?;
        let _registration = Unregister(wide_path(&root)?);
        let connection = CloudFiles::connect(&root, Arc::new(MemoryRemote))?;

        let key = format!("{}/deep.bin", "segment".repeat(16));
        ensure!(
            root.as_os_str().len() + key.len() > 260,
            "test path is short"
        );
        let path = root.join(key.replace('/', "\\"));
        create_placeholder(&path, &key, "long", 1024, 1_700_000_000_000)?;
        let (state, identity) = inspect_entry(&path)?;
        ensure!(
            state.placeholder && state.in_sync,
            "deep placeholder: {state:?}"
        );
        ensure!(
            identity.is_some_and(|identity| identity.key == key && identity.etag == "long"),
            "deep placeholder identity was not persisted"
        );
        ensure!(
            std::fs::read(&path)? == [0x7f; 1024],
            "deep placeholder did not hydrate"
        );
        dehydrate(&path)?;
        ensure!(
            inspect(&path)?.hydrated_bytes == 0,
            "deep placeholder stayed cached"
        );
        hydrate(&path)?;
        ensure!(
            inspect(&path)?.hydrated_bytes > 0,
            "deep placeholder did not rehydrate"
        );
        remove_clean(&path)?;
        ensure!(!path.exists(), "deep placeholder was not deleted");
        drop(connection);
        Ok(())
    }

    #[test]
    #[ignore = "child-process helper for live_cloud_files_round_trip"]
    fn live_cloud_files_external_rename_helper() -> Result<()> {
        let Some(source) = std::env::var_os("BUCKET_MOUNT_CFAPI_RENAME_SOURCE") else {
            return Ok(());
        };
        let source = std::path::PathBuf::from(source);
        let parent = source.parent().context("test file has no parent")?;
        ensure!(
            parent.file_name().is_some_and(|name| name
                .to_string_lossy()
                .starts_with(".bucket-mount-cfapi-test-")),
            "rename helper only accepts a disposable CFAPI test root"
        );
        let renamed = parent.join("renamed.bin");
        std::fs::rename(&source, &renamed)?;
        std::fs::rename(&renamed, &source)?;
        Ok(())
    }
}
