// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ELF loading for user programs.

use alloc::{borrow::ToOwned, string::String, sync::Arc, vec, vec::Vec};
use core::{ffi::CStr, iter};

use filemap::new_file_private_vma;
use kcred::Cred;
use kernel_elf_parser::{AuxEntry, AuxType, ElfHeaders, ElfHeadersBuilder, app_stack_region};
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

/// Validated image and owned initial stack, ready to install into an empty user MM.
///
/// Executable/interpreter files remain pinned independently of the ELF cache.
/// Arguments, environment and execfn are encoded in the owned stack bytes.
/// This value holds no loader lock and does not refer to a target MM.
pub struct PreparedExecImage {
    executable: PreparedElfImage,
    interpreter: Option<PreparedElfImage>,
    layout: ExecLayoutPlan,
    stack_data: Vec<u8>,
    entry: VirtAddr,
    user_sp: VirtAddr,
    #[cfg(unittest)]
    partial_probe: Option<PartialInstallProbe>,
}

/// Request-local partial-install evidence; absent from non-unittest builds.
#[cfg(unittest)]
pub struct PartialInstallProbe {
    /// Counts successful arrivals after ELF mapping, before stack/heap mapping.
    pub hits: Arc<core::sync::atomic::AtomicUsize>,
    /// Scalar handshake with an independent old-MM peer.
    pub stage: Arc<core::sync::atomic::AtomicUsize>,
    /// Optional destination validated by the caller against its original eligible CPUs.
    pub target_cpu: Option<usize>,
    /// Observes borrowed mappings and schedules without reacquiring this MM.
    /// The peer must never require this MM mutex or an inverse lock.
    pub observe: fn(&MmSpace, &core::sync::atomic::AtomicUsize, Option<usize>),
}

#[cfg(unittest)]
impl PreparedExecImage {
    /// Selects a single partial-install failure for this owned image only.
    pub fn with_partial_install_probe(mut self, probe: PartialInstallProbe) -> Self {
        self.partial_probe = Some(probe);
        self
    }
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

        let flags = mapping_flags(segment.flags);
        if !segment.has_file_data && segment.file_start >= file.size() {
            // Only discard the page prefix when it lies beyond EOF; an in-file
            // prefix may carry bytes from an earlier segment sharing this page.
            let start = VirtAddr::from_usize(mapped_start);
            uspace.map(
                start,
                segment.mapping_size(),
                flags,
                false,
                VmRuntimeRef::new_anon_private(start, PageSize::Size4K),
            )?;
            continue;
        }

        // PT_LOAD mappings follow the Linux rule that both VMA start and file
        // offset are aligned down to the page boundary. The page prefix before
        // `p_vaddr` still belongs to the mapped file object and must not be
        // silently zero-filled.
        let (vma, runtime) = new_file_private_vma(
            VirtAddr::from_usize(mapped_start),
            segment.mapping_size(),
            PageSize::Size4K,
            file.clone(),
            segment.file_start,
            Some(segment.file_data_end.min(file.size())),
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
    elf: ElfHeaders<'this>,
}

impl ElfCacheEntry {
    fn load_file(file: Arc<VfsFile>) -> KResult<Result<Self, Vec<u8>>> {
        let initial_size = usize::try_from(file.size().min(ELF_HEADER_READ_SIZE as u64))
            .map_err(|_| KError::InvalidExecutable)?;
        let mut data = vec![0; initial_size];
        read_exact_at(&file, &mut data, 0)?;
        match ElfCacheEntry::try_new_or_recover::<KError>(file.clone(), data, |data| {
            let builder = ElfHeadersBuilder::new(data).map_err(map_elf_error)?;
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

    fn prepare_binprm(
        binprm: BinPrm,
        original_execfn: Option<&str>,
    ) -> KResult<PreparedImageResult> {
        let mut loader = ELF_LOADER.lock();
        match loader.ensure_cached(binprm.executable().clone())? {
            Ok(_) => {}
            Err(data) => return Ok(Err((binprm, data))),
        }

        let executable_entry = loader
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
            match loader.ensure_cached(file.clone())? {
                Ok(_) => Some(
                    loader
                        .cached_entry(file.path())
                        .ok_or(KError::InvalidExecutable)?,
                ),
                Err(_) => return Err(KError::InvalidExecutable),
            }
        } else {
            None
        };
        drop(loader);

        let executable = PreparedElfImage::new(executable_entry)?;
        let interpreter = interpreter_entry.map(PreparedElfImage::new).transpose()?;
        let layout = ExecLayoutPlan::new(
            &executable.image,
            interpreter.as_ref().map(|interpreter| &interpreter.image),
        )?;
        let entry = exec_entry(&executable, interpreter.as_ref(), &layout)?;
        let auxv = initial_aux_vector(&executable.image, &layout, binprm.cred())?;
        let (user_sp, stack_data) = prepare_initial_stack(
            binprm.args(),
            binprm.envs(),
            &auxv,
            original_execfn.unwrap_or(binprm.display_path()),
        )?;
        Ok(Ok(PreparedExecImage {
            executable,
            interpreter,
            layout,
            stack_data,
            entry,
            user_sp,
            #[cfg(unittest)]
            partial_probe: None,
        }))
    }
}

fn map_prepared_image(uspace: &mut MmSpace, prepared: &PreparedExecImage) -> KResult {
    let executable_bias = prepared.layout.executable_bias();

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

fn exec_entry(
    executable: &PreparedElfImage,
    interpreter: Option<&PreparedElfImage>,
    layout: &ExecLayoutPlan,
) -> KResult<VirtAddr> {
    let executable_bias = layout.executable_bias();
    let entry = if let (Some(interpreter), Some(interpreter_bias)) =
        (interpreter, layout.interpreter_bias())
    {
        interpreter.image.entry(interpreter_bias)?
    } else {
        executable.image.entry(executable_bias)?
    };
    Ok(VirtAddr::from_usize(entry))
}

fn initial_aux_vector(
    image: &ValidatedElfImage,
    layout: &ExecLayoutPlan,
    cred: &Cred,
) -> KResult<Vec<AuxEntry>> {
    let executable_bias = layout.executable_bias();
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
    if let Some(interpreter_bias) = layout.interpreter_bias() {
        auxv.push(AuxEntry::new(AuxType::BASE, interpreter_bias));
    }
    auxv.extend(identity_aux_entries(cred));
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

fn prepare_initial_stack(
    args: &[String],
    envs: &[String],
    auxv: &[AuxEntry],
    execfn: &str,
) -> KResult<(VirtAddr, Vec<u8>)> {
    let ustack_top = VirtAddr::from_usize(kaddr_layout::USER_STACK_TOP);
    let ustack_size = kaddr_layout::USER_STACK_SIZE;
    let mut random_bytes = [0_u8; 16];
    entropy::fill_random(&mut random_bytes);
    let stack_data = app_stack_region(args, envs, auxv, ustack_top.into(), &random_bytes, execfn)
        .map_err(|_| KError::ArgumentListTooLong)?;
    if stack_data.len() > ustack_size {
        return Err(KError::ArgumentListTooLong);
    }
    let user_sp = ustack_top - stack_data.len();

    Ok((user_sp, stack_data))
}

/// Resolves and validates an executable, including its complete initial stack.
///
/// Requires sleepable task context and initialized filesystem/entropy services.
/// Kernel callers use the initial filesystem context; no current user MM is
/// required or modified. The result owns its pinned files and encoded argv/env.
///
/// # Errors
///
/// Propagates VFS, signature, ELF/layout and interpreter errors. Script recursion
/// is bounded; oversized stacks return `ArgumentListTooLong`. No target is modified.
pub fn prepare_user_app_request(request: ExecRequest) -> KResult<PreparedExecImage> {
    let mut script_depth = 0;
    let mut request = request;
    let mut original_execfn = None;
    loop {
        let binprm = request.prepare()?;
        match ElfLoader::prepare_binprm(binprm, original_execfn.as_deref())? {
            Ok(prepared) => return Ok(prepared),
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

/// Installs a prepared image into an exclusively borrowed, user-empty MM.
///
/// May allocate and block in sleepable task context. The caller supplies valid
/// kernel mappings; this function neither clears old mappings nor switches MM,
/// publishes a process, or selects fatal-error policy. No current user is needed.
///
/// # Returns
///
/// The entry point and initial user stack pointer, in that order.
///
/// # Errors
///
/// Returns `InvalidInput` without mapping if user VMAs already exist. Mapping,
/// population and write errors can leave a partially installed image; the caller
/// must discard it or apply its post-commit failure policy, not resume old code.
pub fn load_prepared_user_app(
    uspace: &mut MmSpace,
    prepared: PreparedExecImage,
) -> KResult<(VirtAddr, VirtAddr)> {
    if uspace.vmas().next().is_some() {
        return Err(KError::InvalidInput);
    }
    map_prepared_image(uspace, &prepared)?;
    #[cfg(unittest)]
    if let Some(probe) = &prepared.partial_probe {
        assert!(uspace.find_vma(prepared.entry).is_some_and(|vma| {
            vma.flags()
                .contains(MappingFlags::USER | MappingFlags::EXECUTE)
        }));
        assert!(uspace.find_vma(prepared.user_sp).is_none());
        assert!(
            uspace
                .find_vma(kaddr_layout::USER_HEAP_BASE.into())
                .is_none()
        );
        probe
            .hits
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        (probe.observe)(uspace, &probe.stage, probe.target_cpu);
        return Err(KError::NoMemory);
    }
    let ustack_top = VirtAddr::from_usize(kaddr_layout::USER_STACK_TOP);
    let ustack_start = ustack_top - kaddr_layout::USER_STACK_SIZE;
    let user_sp = prepared.user_sp;

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
    uspace.write(user_sp, &prepared.stack_data)?;

    let heap_start = VirtAddr::from_usize(kaddr_layout::USER_HEAP_BASE);
    let heap_size = kaddr_layout::USER_HEAP_SIZE;
    uspace.map(
        heap_start,
        heap_size,
        MappingFlags::READ | MappingFlags::WRITE | MappingFlags::USER,
        true,
        VmRuntimeRef::new_anon_private(heap_start, PageSize::Size4K),
    )?;

    Ok((prepared.entry, user_sp))
}

#[unittest::mod_test]
mod tests {
    use alloc::{borrow::ToOwned, string::String, sync::Arc, vec, vec::Vec};

    use kernel_elf_parser::AuxType;
    use kerrno::KError;
    use khal::paging::MappingFlags;
    use kvfs::{
        Dentry, DirMapping, FileSystemType, Mount, NodeFlags, NodePermission, Path, SimpleDir,
        SimpleFile, SimpleFs, VfsInode,
    };
    use memaddr::VirtAddr;
    use memspace::{MmSpace, VmBackingKind};
    use unittest::def_test;
    use xmas_elf::program::{FLAG_R, FLAG_W, FLAG_X, Flags};

    use super::{
        ExecRequest, ExecSource, identity_aux_entries, mapping_flags, script_interpreter_args,
    };
    use crate::elf_image::tests::{
        executable_bytes, probe_bss_fixture, shared_page_bss_crosses_eof_fixture,
        shared_page_bss_fixture,
    };

    static TEST_FS: FileSystemType = FileSystemType::internal("kexec-test");

    fn image_request(data: Vec<u8>, args: Vec<String>) -> ExecRequest {
        let cached_data = data.clone();
        let size = data.len() as u64;
        let mut inode = None;
        let super_block = SimpleFs::new_with(&TEST_FS, 0, |fs| {
            let file = SimpleFile::new_regular_with_permission(
                fs.clone(),
                NodePermission::from_bits_truncate(0o555),
                move || Ok(data.clone()),
            );
            // Scalar reads expose the bytes; the loader also needs their inode size.
            let init = file.inode_init().with_size(size);
            inode = Some(VfsInode::new_file_with_operations(
                file.clone(),
                kvfs::empty_inode_operations(),
                file,
                NodeFlags::ALWAYS_CACHE,
                init,
            ));
            SimpleDir::new_maker(fs, Arc::new(DirMapping::new()))
        });
        let inode = inode.unwrap();
        let mount = Mount::new_root(&super_block);
        let dentry = Dentry::new_file_from_inode(inode, None, "app".into());
        let location = Path::new(mount, dentry);
        let file = super::open_exec_file(&location, kcred::initial_cred()).unwrap();
        for (index, bytes) in cached_data.chunks(4096).enumerate() {
            file.mapping()
                .with_folio_or_create(index as u64, |folio| {
                    folio.data()[..bytes.len()].copy_from_slice(bytes);
                    Ok(())
                })
                .expect("cache fixture bytes");
        }
        ExecRequest::from_resolved_with_display(
            location,
            "/fixture/app".into(),
            args,
            vec!["EXEC_TEST=owned".into()],
            kcred::initial_cred(),
        )
    }

    #[def_test]
    fn loads_pure_bss_beyond_eof_as_zeroed_anonymous_memory() {
        let prepared =
            super::prepare_user_app_request(image_request(probe_bss_fixture(0), vec![])).unwrap();
        let mut mm = MmSpace::new_user_empty().unwrap();
        super::load_prepared_user_app(&mut mm, prepared).unwrap();
        let start = VirtAddr::from_usize(0x41f000);
        let vma = mm.find_vma(start).unwrap();
        assert_eq!(vma.start(), start);
        assert_eq!(vma.end(), VirtAddr::from_usize(0x421000));
        assert_eq!(
            vma.flags(),
            MappingFlags::READ | MappingFlags::WRITE | MappingFlags::USER
        );
        assert!(matches!(
            vma.backing().kind(),
            VmBackingKind::AnonymousPrivate { .. }
        ));
        mm.populate_area(start, 0x2000, MappingFlags::READ).unwrap();
        let mut bytes = vec![0xff; 0x2000];
        mm.read(start, &mut bytes).unwrap();
        assert!(bytes.iter().all(|byte| *byte == 0));
    }

    #[def_test]
    fn pure_bss_preserves_shared_page_file_bytes_and_program_headers() {
        let data = shared_page_bss_fixture();
        let expected = data[0x1000..0x1800].to_vec();
        let prepared = super::prepare_user_app_request(image_request(data, vec![])).unwrap();
        let mut mm = MmSpace::new_user_empty().unwrap();
        super::load_prepared_user_app(&mut mm, prepared).unwrap();
        let start = VirtAddr::from_usize(0x401000);
        assert!(matches!(
            mm.find_vma(start).unwrap().backing().kind(),
            VmBackingKind::FilePrivate { .. }
        ));
        mm.populate_area(start, 0x1000, MappingFlags::READ).unwrap();
        let mut bytes = vec![0xff; 0x1000];
        mm.read(start, &mut bytes).unwrap();
        assert_eq!(&bytes[..0x800], expected.as_slice());
        assert!(bytes[0x800..].iter().all(|byte| *byte == 0));
    }

    #[def_test]
    fn pure_bss_shared_page_crossing_eof_preserves_only_file_bytes() {
        let data = shared_page_bss_crosses_eof_fixture();
        let expected = data[0x1000..].to_vec();
        let prepared = super::prepare_user_app_request(image_request(data, vec![])).unwrap();
        // Poison the cached bytes past EOF: the loader must not expose them,
        // even if a backing implementation leaves a nonzero folio tail.
        prepared
            .executable
            .cache_entry
            .borrow_file()
            .mapping()
            .with_folio_or_create(1, |folio| {
                folio.data()[0x400..0x800].fill(0xcc);
                Ok(())
            })
            .unwrap();
        let mut mm = MmSpace::new_user_empty().unwrap();
        super::load_prepared_user_app(&mut mm, prepared).unwrap();
        let start = VirtAddr::from_usize(0x401000);
        assert!(matches!(
            mm.find_vma(start).unwrap().backing().kind(),
            VmBackingKind::FilePrivate { .. }
        ));
        mm.populate_area(start, 0x1000, MappingFlags::READ).unwrap();
        let mut bytes = vec![0xff; 0x1000];
        mm.read(start, &mut bytes).unwrap();
        assert_eq!(&bytes[..0x400], expected.as_slice());
        assert!(bytes[0x400..].iter().all(|byte| *byte == 0));
    }

    #[def_test]
    fn prepared_image_survives_cache_clear_and_rejections_preserve_installed_mm() {
        let request = image_request(
            executable_bytes(),
            vec!["app".into(), "owned-argument".into()],
        );
        let prepared = super::prepare_user_app_request(request).expect("prepare fixture");
        assert!(!prepared.stack_data.is_empty());
        assert!(prepared.stack_data.len() <= kaddr_layout::USER_STACK_SIZE);
        assert_eq!(
            prepared.user_sp.as_usize(),
            kaddr_layout::USER_STACK_TOP - prepared.stack_data.len()
        );
        assert_eq!(prepared.user_sp.as_usize() % 16, 0);
        for encoded in [
            b"app\0".as_slice(),
            b"owned-argument\0",
            b"EXEC_TEST=owned\0",
            b"/fixture/app\0",
        ] {
            assert!(
                prepared
                    .stack_data
                    .windows(encoded.len())
                    .any(|s| s == encoded)
            );
        }
        assert!(super::ELF_LOADER.try_lock().is_some());
        super::clear_elf_cache();
        let expected_stack = prepared.stack_data.clone();
        let expected_entry = prepared.entry;
        let expected_sp = prepared.user_sp;
        let mut mm = MmSpace::new_user_empty().expect("empty user MM");
        assert!(mm.vmas().next().is_none());
        assert_eq!(
            super::load_prepared_user_app(&mut mm, prepared),
            Ok((expected_entry, expected_sp))
        );
        // Inspect the installed ELF mapping permissions.
        assert!(
            mm.find_vma(expected_entry)
                .unwrap()
                .flags()
                .contains(MappingFlags::USER | MappingFlags::EXECUTE)
        );
        assert!(
            mm.find_vma(VirtAddr::from_usize(kaddr_layout::USER_HEAP_BASE))
                .is_some()
        );
        let mut actual_stack = vec![0; expected_stack.len()];
        mm.read(expected_sp, &mut actual_stack).unwrap();
        assert_eq!(actual_stack, expected_stack);
        let vma_count = mm.vmas().count();

        let second = super::prepare_user_app_request(image_request(executable_bytes(), vec![]))
            .expect("prepare second fixture");
        assert_eq!(
            super::load_prepared_user_app(&mut mm, second),
            Err(KError::InvalidInput)
        );
        assert_eq!(mm.vmas().count(), vma_count);
        actual_stack.fill(0);
        mm.read(expected_sp, &mut actual_stack).unwrap();
        assert_eq!(actual_stack, expected_stack);

        for (data, args, error) in [
            (b"not ELF".to_vec(), vec![], KError::InvalidExecutable),
            (
                executable_bytes(),
                vec!["x".repeat(kaddr_layout::USER_STACK_SIZE)],
                KError::ArgumentListTooLong,
            ),
        ] {
            assert!(matches!(
                super::prepare_user_app_request(image_request(data, args)),
                Err(actual) if actual == error
            ));
            assert_eq!(mm.vmas().count(), vma_count);
            actual_stack.fill(0);
            mm.read(expected_sp, &mut actual_stack).unwrap();
            assert_eq!(actual_stack, expected_stack);
        }
        super::clear_elf_cache();
    }

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
