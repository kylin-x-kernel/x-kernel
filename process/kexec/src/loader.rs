// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ELF loading for user programs.

use alloc::{borrow::ToOwned, string::String, sync::Arc, vec, vec::Vec};
use core::{ffi::CStr, fmt, iter};

use filemap::new_file_private_vma;
use kcred::Cred;
use kernel_elf_parser::{AuxEntry, AuxType, ELFHeaders, ELFHeadersBuilder, app_stack_region};
use kerrno::{KError, KResult};
use khal::paging::{MappingFlags, PageSize};
use ksync::{Mutex, static_lock};
use kvfs::{Filename, LookupFlags, LookupIntent, Path, Permission, VfsFile, dentry_open};
use memaddr::{MemoryAddr, PAGE_SIZE_4K, VirtAddr};
use memspace::{MmSpace, VmRuntimeRef};
use ouroboros::self_referencing;

use super::{
    elf_image::{ExecLayoutPlan, ValidatedElfImage},
    lru_cache::LruCache,
};

const SCRIPT_RECURSION_MAX: usize = 4;
const ELF_HEADER_READ_SIZE: usize = 4096;
const INTERPRETER_PATH_MAX: usize = 4096;

/// Failure while replacing a user address space.
///
/// The variants identify whether the old user image still exists. Callers may
/// return [`Self::BeforeCommit`] to the old program as an ordinary exec error.
/// They must terminate the task after [`Self::AfterCommit`] because the old
/// image has already been discarded and cannot safely resume.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecFailure {
    /// Loading failed before `MmSpace::clear`; the old image is intact.
    BeforeCommit(KError),
    /// Loading failed after `MmSpace::clear`; the old image is gone.
    AfterCommit(KError),
}

impl ExecFailure {
    /// Returns the underlying kernel error.
    pub const fn into_error(self) -> KError {
        match self {
            Self::BeforeCommit(error) | Self::AfterCommit(error) => error,
        }
    }
}

impl fmt::Display for ExecFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BeforeCommit(error) => write!(formatter, "exec failed before commit: {error}"),
            Self::AfterCommit(error) => write!(formatter, "exec failed after commit: {error}"),
        }
    }
}

fn mapping_flags(flags: xmas_elf::program::Flags) -> MappingFlags {
    let mut mapping_flags = MappingFlags::USER;
    if flags.is_read() {
        mapping_flags |= MappingFlags::READ;
    }
    if flags.is_write() {
        mapping_flags |= MappingFlags::WRITE;
    }
    if flags.is_execute() {
        mapping_flags |= MappingFlags::EXECUTE;
    }
    mapping_flags
}

fn open_exec_file(location: &Path, cred: Arc<Cred>) -> KResult<Arc<VfsFile>> {
    dentry_open(location.clone(), 0, cred)
}

/// Source of an executable image.
#[derive(Clone)]
pub enum ExecSource {
    /// Resolve the executable from the caller's current filesystem context.
    Path(String),
    /// Use an already-resolved VFS location as the executable.
    Resolved {
        /// Resolved executable location.
        location: Path,
        /// Display path used for process metadata and script argv rewriting.
        display_path: Option<String>,
    },
}

impl ExecSource {
    fn resolve(&self, cred: &Cred) -> KResult<(Path, String)> {
        match self {
            Self::Path(path) => {
                let fs_struct = kprocess::current_fs_context();
                let fs = fs_struct.lock();
                let location = Filename::new(path.as_str()).lookup_at(
                    fs.root(),
                    fs.pwd(),
                    LookupIntent::Exec,
                    LookupFlags::follow(),
                    cred,
                )?;
                location.permission(Permission::MAY_EXEC, cred)?;
                Ok((location, path.clone()))
            }
            Self::Resolved {
                location,
                display_path,
            } => {
                location.permission(Permission::MAY_EXEC, cred)?;
                let display_path = match display_path {
                    Some(path) => path.clone(),
                    None => location.absolute_path()?.as_str().to_owned(),
                };
                Ok((location.clone(), display_path))
            }
        }
    }
}

/// Owned exec request before executable resolution.
pub struct ExecRequest {
    source: ExecSource,
    args: Vec<String>,
    envs: Vec<String>,
    cred: Arc<Cred>,
}

impl ExecRequest {
    /// Creates an exec request from a path string.
    pub fn from_path(
        path: impl Into<String>,
        args: Vec<String>,
        envs: Vec<String>,
        cred: Arc<Cred>,
    ) -> Self {
        Self {
            source: ExecSource::Path(path.into()),
            args,
            envs,
            cred,
        }
    }

    /// Creates an exec request from an already-resolved executable location.
    pub fn from_resolved(
        location: Path,
        args: Vec<String>,
        envs: Vec<String>,
        cred: Arc<Cred>,
    ) -> Self {
        Self {
            source: ExecSource::Resolved {
                location,
                display_path: None,
            },
            args,
            envs,
            cred,
        }
    }

    /// Creates an exec request from a resolved executable plus display path.
    pub fn from_resolved_with_display(
        location: Path,
        display_path: String,
        args: Vec<String>,
        envs: Vec<String>,
        cred: Arc<Cred>,
    ) -> Self {
        Self {
            source: ExecSource::Resolved {
                location,
                display_path: Some(display_path),
            },
            args,
            envs,
            cred,
        }
    }

    /// Returns the requested executable source.
    pub fn source(&self) -> &ExecSource {
        &self.source
    }

    /// Returns the owned argument vector.
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// Returns the owned environment vector.
    pub fn envs(&self) -> &[String] {
        &self.envs
    }

    /// Resolves the executable and creates a binprm object without mutating
    /// the target address space.
    ///
    /// # Errors
    ///
    /// Propagates VFS lookup, execute-permission, absolute-path reconstruction, and
    /// file-open errors. Both resolved and string sources undergo an execute
    /// permission check using the supplied credential snapshot. No address space is modified.
    pub fn prepare(self) -> KResult<BinPrm> {
        let (location, display_path) = self.source.resolve(&self.cred)?;
        let executable = open_exec_file(&location, self.cred.clone())?;
        Ok(BinPrm {
            location,
            executable,
            display_path,
            args: self.args,
            envs: self.envs,
            cred: self.cred,
        })
    }
}

/// Prepared executable image state.
pub struct BinPrm {
    location: Path,
    executable: Arc<VfsFile>,
    display_path: String,
    args: Vec<String>,
    envs: Vec<String>,
    cred: Arc<Cred>,
}

impl BinPrm {
    /// Returns the executable location.
    pub fn location(&self) -> &Path {
        &self.location
    }

    /// Returns the opened executable file.
    pub fn executable(&self) -> &Arc<VfsFile> {
        &self.executable
    }

    fn cred(&self) -> &Arc<Cred> {
        &self.cred
    }

    /// Returns the display path used for argv/script reconstruction.
    pub fn display_path(&self) -> &str {
        &self.display_path
    }

    /// Returns the owned argument vector.
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// Returns the owned environment vector.
    pub fn envs(&self) -> &[String] {
        &self.envs
    }
}

struct PreparedExecImage {
    binprm: BinPrm,
    execfn: String,
    executable: PreparedElfImage,
    interpreter: Option<PreparedElfImage>,
    layout: ExecLayoutPlan,
}

struct PreparedElfImage {
    cache_entry: Arc<ElfCacheEntry>,
    image: ValidatedElfImage,
}

impl PreparedElfImage {
    fn new(cache_entry: Arc<ElfCacheEntry>) -> KResult<Self> {
        let image =
            ValidatedElfImage::new(cache_entry.borrow_elf(), cache_entry.borrow_file().size())?;
        Ok(Self { cache_entry, image })
    }
}

fn map_elf(uspace: &mut MmSpace, load_bias: usize, prepared: &PreparedElfImage) -> KResult {
    let file = prepared.cache_entry.borrow_file();

    for segment in prepared.image.segments() {
        let mapped_start = load_bias
            .checked_add(segment.mapping_start)
            .ok_or(KError::InvalidExecutable)?;
        let mapped_end = load_bias
            .checked_add(segment.mapping_end)
            .ok_or(KError::InvalidExecutable)?;
        debug!(
            "Mapping ELF segment: [{:#x?}, {:#x?}) flags: {}",
            mapped_start, mapped_end, segment.flags
        );

        // PT_LOAD mappings follow the Linux rule that both VMA start and file
        // offset are aligned down to the page boundary. The page prefix before
        // `p_vaddr` still belongs to the mapped file object and must not be
        // silently zero-filled.
        let flags = mapping_flags(segment.flags);
        let (vma, runtime) = new_file_private_vma(
            VirtAddr::from_usize(mapped_start),
            segment.mapping_size(),
            PageSize::Size4K,
            file.clone(),
            segment.file_start,
            Some(segment.file_data_end),
            flags,
        )?;
        uspace.map_runtime_vma(vma, false, runtime)?;
    }

    Ok(())
}

fn map_elf_error(err: &'static str) -> KError {
    debug!("Failed to parse ELF file: {err}");
    KError::InvalidExecutable
}

#[self_referencing]
struct ElfCacheEntry {
    file: Arc<VfsFile>,
    data: Vec<u8>,
    #[borrows(data)]
    #[covariant]
    elf: ELFHeaders<'this>,
}

impl ElfCacheEntry {
    fn load_file(file: Arc<VfsFile>) -> KResult<Result<Self, Vec<u8>>> {
        let initial_size = usize::try_from(file.size().min(ELF_HEADER_READ_SIZE as u64))
            .map_err(|_| KError::InvalidExecutable)?;
        let mut data = vec![0; initial_size];
        read_exact_at(&file, &mut data, 0)?;
        match ElfCacheEntry::try_new_or_recover::<KError>(file.clone(), data, |data| {
            let builder = ELFHeadersBuilder::new(data).map_err(map_elf_error)?;
            let range = builder.ph_range().map_err(map_elf_error)?;
            if range.end > file.size() {
                return Err(KError::InvalidExecutable);
            }
            let range_start =
                usize::try_from(range.start).map_err(|_| KError::InvalidExecutable)?;
            let range_end = usize::try_from(range.end).map_err(|_| KError::InvalidExecutable)?;
            if range_end <= data.len() {
                builder.build(&data[range_start..range_end])
            } else {
                let range_len = range_end
                    .checked_sub(range_start)
                    .ok_or(KError::InvalidExecutable)?;
                let mut buf = vec![0; range_len];
                read_exact_at(&file, &mut buf, range.start)?;
                builder.build(&buf)
            }
            .map_err(map_elf_error)
        }) {
            Ok(entry) => {
                #[cfg(feature = "tee_ta_sign")]
                {
                    tee_task_iface::tasign::verify_ta_elf_on_load_and_cache_ta_head(
                        entry.borrow_file(),
                    )
                    .map_err(|_err| KError::PermissionDenied)?;
                }
                Ok(Ok(entry))
            }
            Err((_, heads)) => Ok(Err(heads.data)),
        }
    }
}

fn read_exact_at(file: &VfsFile, output: &mut [u8], offset: u64) -> KResult {
    let mut position = offset;
    let mut filled = 0;
    while filled < output.len() {
        let read = file.read_from(&mut output[filled..], &mut position)?;
        if read == 0 {
            return Err(KError::InvalidExecutable);
        }
        filled = filled.checked_add(read).ok_or(KError::InvalidExecutable)?;
    }
    Ok(())
}

struct ElfLoader(LruCache<Arc<ElfCacheEntry>, 32>);
type CacheProbeResult = Result<(), Vec<u8>>;
type PreparedImageResult = Result<PreparedExecImage, (BinPrm, Vec<u8>)>;

impl ElfLoader {
    const fn new() -> Self {
        Self(LruCache::new())
    }

    fn access_cached(&mut self, loc: &Path) -> bool {
        if !self
            .0
            .access(|entry| entry.borrow_file().path().ptr_eq(loc))
        {
            return false;
        }
        true
    }

    fn cached_entry(&self, loc: &Path) -> Option<Arc<ElfCacheEntry>> {
        self.0
            .items()
            .find(|entry| entry.borrow_file().path().ptr_eq(loc))
            .cloned()
    }

    fn ensure_cached(&mut self, file: Arc<VfsFile>) -> KResult<CacheProbeResult> {
        if !self.access_cached(file.path()) {
            match ElfCacheEntry::load_file(file)? {
                Ok(entry) => {
                    self.0.put(Arc::new(entry));
                }
                Err(data) => return Ok(Err(data)),
            }
        }
        Ok(Ok(()))
    }

    fn interp_path(entry: &ElfCacheEntry) -> KResult<Option<String>> {
        let mut interpreters = entry
            .borrow_elf()
            .ph
            .iter()
            .filter(|header| header.get_type() == Ok(xmas_elf::program::Type::Interp));
        let Some(header) = interpreters.next() else {
            return Ok(None);
        };
        if interpreters.next().is_some() {
            return Err(KError::InvalidExecutable);
        }

        let path_len = usize::try_from(header.file_size).map_err(|_| KError::InvalidExecutable)?;
        if path_len == 0 || path_len > INTERPRETER_PATH_MAX {
            return Err(KError::InvalidExecutable);
        }
        let end = header
            .offset
            .checked_add(header.file_size)
            .ok_or(KError::InvalidExecutable)?;

        let file = entry.borrow_file();
        if end > file.size() {
            return Err(KError::InvalidExecutable);
        }
        let mut data = vec![0; path_len];
        read_exact_at(file, &mut data, header.offset)?;

        let ldso = CStr::from_bytes_with_nul(&data)
            .ok()
            .and_then(|cstr| cstr.to_str().ok())
            .ok_or(KError::InvalidExecutable)?;
        Ok(Some(ldso.to_owned()))
    }

    fn prepare_binprm(&mut self, binprm: BinPrm) -> KResult<PreparedImageResult> {
        match self.ensure_cached(binprm.executable().clone())? {
            Ok(_) => {}
            Err(data) => return Ok(Err((binprm, data))),
        }

        let executable_entry = self
            .cached_entry(binprm.location())
            .ok_or(KError::InvalidExecutable)?;
        let interpreter_path = Self::interp_path(&executable_entry)?;
        let interpreter_entry = if let Some(ldso) = interpreter_path {
            debug!("Loading dynamic linker: {ldso}");
            let fs_struct = kprocess::current_fs_context();
            let fs = fs_struct.lock();
            let location = Filename::new(ldso.as_str()).lookup_at(
                fs.root(),
                fs.pwd(),
                LookupIntent::Exec,
                LookupFlags::follow(),
                binprm.cred(),
            )?;
            location.permission(Permission::MAY_EXEC, binprm.cred())?;
            let file = open_exec_file(&location, binprm.cred().clone())?;
            match self.ensure_cached(file.clone())? {
                Ok(_) => Some(
                    self.cached_entry(file.path())
                        .ok_or(KError::InvalidExecutable)?,
                ),
                Err(_) => return Err(KError::InvalidExecutable),
            }
        } else {
            None
        };

        let executable = PreparedElfImage::new(executable_entry)?;
        let interpreter = interpreter_entry.map(PreparedElfImage::new).transpose()?;
        let layout = ExecLayoutPlan::new(
            &executable.image,
            interpreter.as_ref().map(|interpreter| &interpreter.image),
        )?;
        let execfn = binprm.display_path().to_owned();
        Ok(Ok(PreparedExecImage {
            binprm,
            execfn,
            executable,
            interpreter,
            layout,
        }))
    }

    fn commit_prepared_binprm(uspace: &mut MmSpace, prepared: &PreparedExecImage) -> KResult {
        let executable_bias = prepared.layout.executable_bias();

        // Point of no return: from here on the old user image is discarded and
        // all remaining work must consume prevalidated, already-pinned objects.
        uspace.clear();
        ksignal::map_signal_trampoline(uspace)?;

        map_elf(uspace, executable_bias, &prepared.executable)?;
        if let (Some(interpreter), Some(interpreter_bias)) = (
            prepared.interpreter.as_ref(),
            prepared.layout.interpreter_bias(),
        ) {
            map_elf(uspace, interpreter_bias, interpreter)?;
        }

        Ok(())
    }
}

fn exec_entry(prepared: &PreparedExecImage) -> KResult<VirtAddr> {
    let executable_bias = prepared.layout.executable_bias();
    let entry = if let (Some(interpreter), Some(interpreter_bias)) = (
        prepared.interpreter.as_ref(),
        prepared.layout.interpreter_bias(),
    ) {
        interpreter.image.entry(interpreter_bias)?
    } else {
        prepared.executable.image.entry(executable_bias)?
    };
    Ok(VirtAddr::from_usize(entry))
}

fn initial_aux_vector(prepared: &PreparedExecImage) -> KResult<Vec<AuxEntry>> {
    let executable_bias = prepared.layout.executable_bias();
    let image = &prepared.executable.image;
    let mut auxv = vec![
        AuxEntry::new(
            AuxType::PHDR,
            image.program_header_address(executable_bias)?,
        ),
        AuxEntry::new(AuxType::PHENT, image.program_header_entry_size()),
        AuxEntry::new(AuxType::PHNUM, image.program_header_count()),
        AuxEntry::new(AuxType::PAGESZ, PAGE_SIZE_4K),
        AuxEntry::new(AuxType::ENTRY, image.entry(executable_bias)?),
    ];
    if let Some(interpreter_bias) = prepared.layout.interpreter_bias() {
        auxv.push(AuxEntry::new(AuxType::BASE, interpreter_bias));
    }
    auxv.extend(identity_aux_entries(prepared.binprm.cred()));
    Ok(auxv)
}

fn identity_aux_entries(cred: &Cred) -> [AuxEntry; 5] {
    let is_secure = cred.ruid() != cred.euid() || cred.rgid() != cred.egid();
    [
        AuxEntry::new(AuxType::UID, cred.ruid() as usize),
        AuxEntry::new(AuxType::EUID, cred.euid() as usize),
        AuxEntry::new(AuxType::GID, cred.rgid() as usize),
        AuxEntry::new(AuxType::EGID, cred.egid() as usize),
        AuxEntry::new(AuxType::SECURE, usize::from(is_secure)),
    ]
}

static_lock! {
    static ELF_LOADER: Mutex<ElfLoader> = Mutex::new(ElfLoader::new());
}

/// Clear the ELF cache.
///
/// Useful for removing noise during memory leak detection.
pub fn clear_elf_cache() {
    ELF_LOADER.lock().0.flush();
    #[cfg(feature = "tee_ta_sign")]
    tee_task_iface::tasign::clear_ta_head_cache();
}

/// Load a user app from an owned executable request.
///
/// This is the exec-facing entry point when the caller has already resolved
/// the executable location, for example through a procfs magic link.
///
/// Requires task context, an initialized filesystem context for path/interpreter
/// lookup, and exclusive ownership of `uspace`. It may block on VFS and loader locks.
///
/// # Returns
///
/// Returns `(entry_point, initial_stack_pointer)` after mapping the executable,
/// optional interpreter, signal trampoline, stack, and heap.
///
/// # Errors
///
/// Returns `InvalidExecutable` for a malformed, unsupported-architecture,
/// non-ELF/non-script image or invalid ELF interpreter, `InvalidInput` for a
/// non-UTF-8 or empty script-interpreter line, `FilesystemLoop` after four script
/// redirects, `NoMemory` when the validated images cannot fit below the fixed
/// heap, and `ArgumentListTooLong` for an unrepresentable/oversized initial stack.
/// Propagates VFS and MM mapping/population/write errors, and optional TA-signature
/// rejection (`PermissionDenied`). Errors before `MmSpace::clear` are wrapped in
/// [`ExecFailure::BeforeCommit`]. Mapping, population, or write errors after the
/// clear are wrapped in [`ExecFailure::AfterCommit`]; the caller must terminate
/// the task because the previous user image no longer exists.
///
/// # Panics
///
/// Current-context and filesystem initialization preconditions must hold.
pub fn load_user_app_request(
    uspace: &mut MmSpace,
    request: ExecRequest,
) -> Result<(VirtAddr, VirtAddr), ExecFailure> {
    load_user_app_request_inner(uspace, request, 0)
}

fn script_interpreter_args(
    line: &str,
    script_path: &str,
    original_args: &[String],
) -> KResult<Vec<String>> {
    let args = line
        .trim()
        .splitn(2, |c: char| c.is_ascii_whitespace())
        .map(|s| s.trim_ascii().to_owned())
        .chain(iter::once(script_path.to_owned()))
        .chain(original_args.iter().skip(1).cloned())
        .collect::<Vec<_>>();
    if args.first().is_none_or(String::is_empty) {
        return Err(KError::InvalidInput);
    }
    Ok(args)
}

fn load_user_app_request_inner(
    uspace: &mut MmSpace,
    request: ExecRequest,
    script_depth: usize,
) -> Result<(VirtAddr, VirtAddr), ExecFailure> {
    let prepared =
        prepare_user_app_request(request, script_depth).map_err(ExecFailure::BeforeCommit)?;
    let entry = exec_entry(&prepared).map_err(ExecFailure::BeforeCommit)?;
    let auxv = initial_aux_vector(&prepared).map_err(ExecFailure::BeforeCommit)?;

    let ustack_top = VirtAddr::from_usize(kaddr_layout::USER_STACK_TOP);
    let ustack_size = kaddr_layout::USER_STACK_SIZE;
    let ustack_start = ustack_top - ustack_size;
    let mut random_bytes = [0_u8; 16];
    entropy::fill_random(&mut random_bytes);
    let stack_data = app_stack_region(
        prepared.binprm.args(),
        prepared.binprm.envs(),
        &auxv,
        ustack_top.into(),
        &random_bytes,
        &prepared.execfn,
    )
    .map_err(|_| ExecFailure::BeforeCommit(KError::ArgumentListTooLong))?;
    if stack_data.len() > ustack_size {
        return Err(ExecFailure::BeforeCommit(KError::ArgumentListTooLong));
    }
    let user_sp = ustack_top - stack_data.len();

    commit_user_app_image(
        uspace,
        &prepared,
        ustack_start,
        ustack_top,
        user_sp,
        &stack_data,
    )
    .map_err(ExecFailure::AfterCommit)?;

    Ok((entry, user_sp))
}

fn prepare_user_app_request(
    request: ExecRequest,
    mut script_depth: usize,
) -> KResult<PreparedExecImage> {
    let mut request = request;
    let mut original_execfn = None;
    loop {
        let binprm = request.prepare()?;
        match ELF_LOADER.lock().prepare_binprm(binprm)? {
            Ok(mut prepared) => {
                if let Some(execfn) = original_execfn {
                    prepared.execfn = execfn;
                }
                return Ok(prepared);
            }
            Err((binprm, data)) => {
                if !data.starts_with(b"#!") {
                    return Err(KError::InvalidExecutable);
                }
                if script_depth >= SCRIPT_RECURSION_MAX {
                    return Err(KError::FilesystemLoop);
                }
                let head = &data[2..data.len().min(256)];
                let pos = head
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .unwrap_or(head.len());
                let line = core::str::from_utf8(&head[..pos]).map_err(|_| KError::InvalidInput)?;

                original_execfn.get_or_insert_with(|| binprm.display_path().to_owned());
                let new_args = script_interpreter_args(line, binprm.display_path(), binprm.args())?;
                let interpreter = new_args.first().ok_or(KError::InvalidInput)?.clone();
                request = ExecRequest::from_path(
                    interpreter,
                    new_args,
                    binprm.envs().to_vec(),
                    binprm.cred().clone(),
                );
                script_depth += 1;
            }
        }
    }
}

fn commit_user_app_image(
    uspace: &mut MmSpace,
    prepared: &PreparedExecImage,
    ustack_start: VirtAddr,
    ustack_top: VirtAddr,
    user_sp: VirtAddr,
    stack_data: &[u8],
) -> KResult {
    ElfLoader::commit_prepared_binprm(uspace, prepared)?;

    debug!("Mapping user stack: {ustack_start:#x?} -> {ustack_top:#x?}");

    uspace.map(
        ustack_start,
        kaddr_layout::USER_STACK_SIZE,
        MappingFlags::READ | MappingFlags::WRITE | MappingFlags::USER,
        false,
        VmRuntimeRef::new_anon_private(ustack_start, PageSize::Size4K),
    )?;

    let user_sp_aligned = user_sp.align_down_4k();
    uspace.populate_area(
        user_sp_aligned,
        (ustack_top - user_sp_aligned).align_up_4k(),
        MappingFlags::READ | MappingFlags::WRITE,
    )?;
    uspace.write(user_sp, stack_data)?;

    let heap_start = VirtAddr::from_usize(kaddr_layout::USER_HEAP_BASE);
    let heap_size = kaddr_layout::USER_HEAP_SIZE;
    uspace.map(
        heap_start,
        heap_size,
        MappingFlags::READ | MappingFlags::WRITE | MappingFlags::USER,
        true,
        VmRuntimeRef::new_anon_private(heap_start, PageSize::Size4K),
    )?;

    Ok(())
}

#[cfg(unittest)]
mod tests {
    use alloc::{borrow::ToOwned, vec};

    use kernel_elf_parser::AuxType;
    use kerrno::KError;
    use khal::paging::MappingFlags;
    use unittest::def_test;
    use xmas_elf::program::{FLAG_R, FLAG_W, FLAG_X, Flags};

    use super::{
        ExecRequest, ExecSource, identity_aux_entries, mapping_flags, script_interpreter_args,
    };

    #[def_test]
    fn test_mapping_flags_sets_user_and_requested_permissions() {
        let none = mapping_flags(Flags(0));
        assert_eq!(none, MappingFlags::USER);

        let read = mapping_flags(Flags(FLAG_R));
        assert!(read.contains(MappingFlags::USER | MappingFlags::READ));
        assert!(!read.contains(MappingFlags::WRITE));

        let write_exec = mapping_flags(Flags(FLAG_W | FLAG_X));
        assert!(write_exec.contains(MappingFlags::USER | MappingFlags::WRITE));
        assert!(write_exec.contains(MappingFlags::EXECUTE));
        assert!(!write_exec.contains(MappingFlags::READ));
    }

    #[def_test]
    fn test_mapping_flags_all_bits_combination() {
        let flags = mapping_flags(Flags(FLAG_R | FLAG_W | FLAG_X));
        assert!(flags.contains(MappingFlags::USER));
        assert!(flags.contains(MappingFlags::READ));
        assert!(flags.contains(MappingFlags::WRITE));
        assert!(flags.contains(MappingFlags::EXECUTE));
    }

    #[def_test]
    fn exec_request_owns_path_args_and_envs() {
        let mut args = vec!["app".to_owned(), "one".to_owned()];
        let mut envs = vec!["A=B".to_owned()];
        let request = ExecRequest::from_path(
            "/bin/app",
            args.clone(),
            envs.clone(),
            kcred::initial_cred(),
        );

        args[0].push_str("-changed");
        envs[0].push_str("-changed");

        match request.source() {
            ExecSource::Path(path) => assert_eq!(path, "/bin/app"),
            ExecSource::Resolved { .. } => panic!("unexpected resolved source"),
        }
        assert_eq!(request.args().len(), 2);
        assert_eq!(request.args()[0], "app");
        assert_eq!(request.args()[1], "one");
        assert_eq!(request.envs().len(), 1);
        assert_eq!(request.envs()[0], "A=B");
    }

    #[def_test]
    fn script_interpreter_args_rewrites_linux_shape() {
        let original = vec![
            "/tmp/script.sh".to_owned(),
            "arg1".to_owned(),
            "arg2".to_owned(),
        ];
        let rewritten =
            script_interpreter_args("/bin/sh -e", "/tmp/script.sh", original.as_slice())
                .expect("valid interpreter line");

        assert_eq!(rewritten.len(), 5);
        assert_eq!(rewritten[0], "/bin/sh");
        assert_eq!(rewritten[1], "-e");
        assert_eq!(rewritten[2], "/tmp/script.sh");
        assert_eq!(rewritten[3], "arg1");
        assert_eq!(rewritten[4], "arg2");
    }

    #[def_test]
    fn script_interpreter_args_rejects_empty_line() {
        assert_eq!(
            script_interpreter_args("   ", "/tmp/script.sh", &[]),
            Err(KError::InvalidInput)
        );
    }

    #[def_test]
    fn identity_aux_entries_report_ids_and_secure_exec() {
        let mut cred = kcred::Cred::root();
        cred.set_resgid(Some(100), Some(200), Some(300))
            .expect("root must be able to set group IDs");
        cred.set_resuid(Some(1000), Some(2000), Some(3000))
            .expect("root must be able to set user IDs");

        let entries = identity_aux_entries(&cred);
        assert!(entries[0].get_type() == AuxType::UID);
        assert_eq!(entries[0].value(), 1000);
        assert!(entries[1].get_type() == AuxType::EUID);
        assert_eq!(entries[1].value(), 2000);
        assert!(entries[2].get_type() == AuxType::GID);
        assert_eq!(entries[2].value(), 100);
        assert!(entries[3].get_type() == AuxType::EGID);
        assert_eq!(entries[3].value(), 200);
        assert!(entries[4].get_type() == AuxType::SECURE);
        assert_eq!(entries[4].value(), 1);
    }

    #[def_test]
    fn identity_aux_entries_include_zero_secure_value() {
        let entries = identity_aux_entries(&kcred::Cred::new(1000, 100));
        assert!(entries[4].get_type() == AuxType::SECURE);
        assert_eq!(entries[4].value(), 0);
    }
}
