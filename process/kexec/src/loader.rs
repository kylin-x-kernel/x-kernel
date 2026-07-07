// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ELF loading for user programs.

use alloc::{borrow::ToOwned, string::String, sync::Arc, vec, vec::Vec};
use core::{ffi::CStr, iter};

use filemap::new_file_private_vma;
use kernel_elf_parser::{AuxEntry, ELFHeaders, ELFHeadersBuilder, ELFParser, app_stack_region};
use kerrno::{KError, KResult};
use kfs::{File, OpenOptions};
use khal::paging::{MappingFlags, PageSize};
use ksync::{Mutex, static_lock};
use kvfs::{Location, LookupFlags, LookupIntent, lookup_location};
use memaddr::{MemoryAddr, PAGE_SIZE_4K, VirtAddr};
use memspace::{MmSpace, VmRuntimeRef};
use ouroboros::self_referencing;

use super::lru_cache::LruCache;

const SCRIPT_RECURSION_MAX: usize = 4;

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

fn open_exec_file(location: &Location) -> KResult<Arc<File>> {
    Ok(Arc::new(
        OpenOptions::new().read(true).open_loc(location.clone())?,
    ))
}

/// Source of an executable image.
#[derive(Clone)]
pub enum ExecSource {
    /// Resolve the executable from the caller's current filesystem context.
    Path(String),
    /// Use an already-resolved VFS location as the executable.
    Resolved {
        /// Resolved executable location.
        location: Location,
        /// Display path used for process metadata and script argv rewriting.
        display_path: Option<String>,
    },
}

impl ExecSource {
    fn resolve(&self) -> KResult<(Location, String)> {
        match self {
            Self::Path(path) => {
                let fs_context = kthread::current_fs_context();
                let fs = fs_context.lock();
                let location = lookup_location(
                    &fs.lookup_context(),
                    path.as_str(),
                    LookupIntent::Exec,
                    LookupFlags::follow(),
                )?;
                Ok((location, path.clone()))
            }
            Self::Resolved {
                location,
                display_path,
            } => {
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
}

impl ExecRequest {
    /// Creates an exec request from a path string.
    pub fn from_path(path: impl Into<String>, args: Vec<String>, envs: Vec<String>) -> Self {
        Self {
            source: ExecSource::Path(path.into()),
            args,
            envs,
        }
    }

    /// Creates an exec request from a resolved executable plus display path.
    pub fn from_resolved_with_display(
        location: Location,
        display_path: String,
        args: Vec<String>,
        envs: Vec<String>,
    ) -> Self {
        Self {
            source: ExecSource::Resolved {
                location,
                display_path: Some(display_path),
            },
            args,
            envs,
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
    pub fn prepare(self) -> KResult<BinPrm> {
        let (location, display_path) = self.source.resolve()?;
        let executable = open_exec_file(&location)?;
        Ok(BinPrm {
            location,
            executable,
            display_path,
            args: self.args,
            envs: self.envs,
        })
    }
}

/// Prepared executable image state.
pub struct BinPrm {
    location: Location,
    executable: Arc<File>,
    display_path: String,
    args: Vec<String>,
    envs: Vec<String>,
}

impl BinPrm {
    /// Returns the executable location.
    pub fn location(&self) -> &Location {
        &self.location
    }

    /// Returns the opened executable file.
    pub fn executable(&self) -> &Arc<File> {
        &self.executable
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
    interpreter: Option<Arc<File>>,
}

fn map_elf<'a>(
    uspace: &mut MmSpace,
    base: usize,
    entry: &'a ElfCacheEntry,
) -> KResult<ELFParser<'a>> {
    let elf_parser = ELFParser::new(entry.borrow_elf(), base).map_err(|_| KError::InvalidData)?;
    let file = entry.borrow_file();

    for ph in elf_parser
        .headers()
        .ph
        .iter()
        .filter(|ph| ph.get_type() == Ok(xmas_elf::program::Type::Load))
    {
        let vaddr = ph.virtual_addr as usize + elf_parser.base();
        debug!(
            "Mapping ELF segment: [{:#x?}, {:#x?}) flags: {}",
            vaddr,
            vaddr + ph.mem_size as usize,
            ph.flags
        );
        let seg_pad = vaddr.align_offset_4k();
        assert_eq!(seg_pad, ph.offset as usize % PAGE_SIZE_4K);

        let seg_align_size =
            (ph.mem_size as usize + seg_pad + PAGE_SIZE_4K - 1) & !(PAGE_SIZE_4K - 1);
        let seg_start = VirtAddr::from_usize(vaddr);
        let mapped_start = seg_start.align_down_4k();
        let file_start = (ph.offset as usize).align_down_4k() as u64;

        // PT_LOAD mappings follow the Linux rule that both VMA start and file
        // offset are aligned down to the page boundary. The page prefix before
        // `p_vaddr` still belongs to the mapped file object and must not be
        // silently zero-filled.
        let flags = mapping_flags(ph.flags);
        let (vma, runtime) = new_file_private_vma(
            mapped_start,
            seg_align_size,
            PageSize::Size4K,
            file.clone(),
            file_start,
            Some(ph.offset + ph.file_size),
            flags,
        )?;
        uspace.map_runtime_vma(vma, false, runtime)?;
    }

    Ok(elf_parser)
}

fn map_elf_error(err: &'static str) -> KError {
    debug!("Failed to parse ELF file: {err}");
    KError::InvalidExecutable
}

#[self_referencing]
struct ElfCacheEntry {
    file: Arc<File>,
    data: Vec<u8>,
    #[borrows(data)]
    #[covariant]
    elf: ELFHeaders<'this>,
}

impl ElfCacheEntry {
    fn load_file(file: Arc<File>) -> KResult<Result<Self, Vec<u8>>> {
        let mut data = vec![0; 4096];
        let read = file.read_at(&mut data[..], 0)?;
        data.truncate(read);
        match ElfCacheEntry::try_new_or_recover::<KError>(file.clone(), data, |data| {
            let builder = ELFHeadersBuilder::new(data).map_err(map_elf_error)?;
            let range = builder.ph_range();
            if range.end as usize <= data.len() {
                builder.build(&data[range.start as usize..range.end as usize])
            } else {
                let mut buf = vec![0; (range.end - range.start) as usize];
                file.read_at(&mut buf[..], range.start)?;
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

struct ElfLoader(LruCache<ElfCacheEntry, 32>);
type CacheProbeResult = Result<(), Vec<u8>>;
type PreparedImageResult = Result<PreparedExecImage, (BinPrm, Vec<u8>)>;

impl ElfLoader {
    const fn new() -> Self {
        Self(LruCache::new())
    }

    fn access_cached(&mut self, loc: &Location) -> bool {
        if !self
            .0
            .access(|entry| entry.borrow_file().location().ptr_eq(loc))
        {
            return false;
        }
        true
    }

    fn cached_entry(&self, loc: &Location) -> Option<&ElfCacheEntry> {
        self.0
            .items()
            .find(|entry| entry.borrow_file().location().ptr_eq(loc))
    }

    fn ensure_cached(&mut self, file: Arc<File>) -> KResult<CacheProbeResult> {
        if !self.access_cached(file.location()) {
            match ElfCacheEntry::load_file(file)? {
                Ok(entry) => {
                    self.0.put(entry);
                }
                Err(data) => return Ok(Err(data)),
            }
        }
        Ok(Ok(()))
    }

    fn interp_path(entry: &ElfCacheEntry) -> KResult<Option<String>> {
        let Some(header) = entry
            .borrow_elf()
            .ph
            .iter()
            .find(|ph| ph.get_type() == Ok(xmas_elf::program::Type::Interp))
        else {
            return Ok(None);
        };

        let file = entry.borrow_file();
        let mut data = vec![0; header.file_size as usize];
        let read = file.read_at(&mut data[..], header.offset)?;
        assert_eq!(data.len(), read);

        let ldso = CStr::from_bytes_with_nul(&data)
            .ok()
            .and_then(|cstr| cstr.to_str().ok())
            .ok_or(KError::InvalidInput)?;
        Ok(Some(ldso.to_owned()))
    }

    fn prepare_binprm(&mut self, binprm: BinPrm) -> KResult<PreparedImageResult> {
        match self.ensure_cached(binprm.executable().clone())? {
            Ok(_) => {}
            Err(data) => return Ok(Err((binprm, data))),
        }

        let interpreter = {
            let executable = self
                .cached_entry(binprm.location())
                .expect("executable entry must be cached before exec commit");
            Self::interp_path(executable)?
        };
        let interpreter = if let Some(ldso) = interpreter {
            debug!("Loading dynamic linker: {ldso}");
            let fs_context = kthread::current_fs_context();
            let fs = fs_context.lock();
            let location = lookup_location(
                &fs.lookup_context(),
                ldso.as_str(),
                LookupIntent::Exec,
                LookupFlags::follow(),
            )?;
            let file = open_exec_file(&location)?;
            match self.ensure_cached(file.clone())? {
                Ok(_) => Some(file),
                Err(_) => return Err(KError::InvalidInput),
            }
        } else {
            None
        };
        Ok(Ok(PreparedExecImage {
            binprm,
            interpreter,
        }))
    }

    fn commit_prepared_binprm(
        &mut self,
        uspace: &mut MmSpace,
        prepared: &PreparedExecImage,
    ) -> KResult<(VirtAddr, Vec<AuxEntry>)> {
        // Point of no return: from here on the old user image is discarded and
        // all remaining work must consume prevalidated, already-pinned objects.
        uspace.clear();
        ksignal::map_signal_trampoline(uspace)?;

        let elf = self
            .cached_entry(prepared.binprm.location())
            .expect("prepared executable entry must remain cached while loading");
        let ldso = prepared.interpreter.as_ref().map(|file| {
            self.cached_entry(file.location())
                .expect("prepared interpreter entry must remain cached while loading")
        });

        let elf = map_elf(uspace, kaddr_layout::USER_SPACE_BASE, elf)?;
        let ldso = ldso
            .map(|elf| map_elf(uspace, kaddr_layout::USER_INTERP_BASE, elf))
            .transpose()?;

        let entry = VirtAddr::from_usize(
            ldso.as_ref()
                .map_or_else(|| elf.entry(), |ldso| ldso.entry()),
        );
        let auxv = elf
            .aux_vector(PAGE_SIZE_4K, ldso.map(|elf| elf.base()))
            .collect::<Vec<_>>();

        Ok((entry, auxv))
    }
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

/// Load the user app to the user address space.
///
/// # Arguments
///
/// - `uspace`: The address space of the user app.
/// - `args`: The arguments of the user app. The first argument is the path of
///   the user app.
/// - `envs`: The environment variables of the user app.
///
/// # Returns
///
/// - The entry point of the user app.
/// - The stack pointer of the user app.
pub fn load_user_app(
    uspace: &mut MmSpace,
    path: Option<&str>,
    args: &[String],
    envs: &[String],
) -> KResult<(VirtAddr, VirtAddr)> {
    let path = path
        .or_else(|| args.first().map(String::as_str))
        .ok_or(KError::InvalidInput)?;
    let request = ExecRequest::from_path(path.to_owned(), args.to_vec(), envs.to_vec());
    load_user_app_request_inner(uspace, request, 0)
}

/// Load a user app from an owned executable request.
///
/// This is the exec-facing entry point when the caller has already resolved
/// the executable location, for example through a procfs magic link.
pub fn load_user_app_request(
    uspace: &mut MmSpace,
    request: ExecRequest,
) -> KResult<(VirtAddr, VirtAddr)> {
    load_user_app_request_inner(uspace, request, 0)
}

fn script_interpreter_args(line: &str, script_path: &str, original_args: &[String]) -> Vec<String> {
    line.trim()
        .splitn(2, |c: char| c.is_ascii_whitespace())
        .map(|s| s.trim_ascii().to_owned())
        .chain(iter::once(script_path.to_owned()))
        .chain(original_args.iter().skip(1).cloned())
        .collect()
}

fn load_user_app_request_inner(
    uspace: &mut MmSpace,
    request: ExecRequest,
    mut script_depth: usize,
) -> KResult<(VirtAddr, VirtAddr)> {
    let mut request = request;
    let prepared = loop {
        let binprm = request.prepare()?;
        match ELF_LOADER.lock().prepare_binprm(binprm)? {
            Ok(prepared) => break prepared,
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

                let new_args = script_interpreter_args(line, binprm.display_path(), binprm.args());
                let interpreter = new_args.first().ok_or(KError::InvalidInput)?.clone();
                request = ExecRequest::from_path(interpreter, new_args, binprm.envs().to_vec());
                script_depth += 1;
            }
        }
    };

    let (entry, auxv) = ELF_LOADER
        .lock()
        .commit_prepared_binprm(uspace, &prepared)?;

    let ustack_top = VirtAddr::from_usize(kaddr_layout::USER_STACK_TOP);
    let ustack_size = kaddr_layout::USER_STACK_SIZE;
    let ustack_start = ustack_top - ustack_size;
    debug!("Mapping user stack: {ustack_start:#x?} -> {ustack_top:#x?}");

    uspace.map(
        ustack_start,
        ustack_size,
        MappingFlags::READ | MappingFlags::WRITE | MappingFlags::USER,
        false,
        VmRuntimeRef::new_anon_private(ustack_start, PageSize::Size4K),
    )?;

    let stack_data = app_stack_region(
        prepared.binprm.args(),
        prepared.binprm.envs(),
        &auxv,
        ustack_top.into(),
    )
    .map_err(|_| KError::ArgumentListTooLong)?;
    if stack_data.len() > ustack_size {
        return Err(KError::ArgumentListTooLong);
    }
    let user_sp = ustack_top - stack_data.len();
    let user_sp_aligned = user_sp.align_down_4k();
    uspace.populate_area(
        user_sp_aligned,
        (ustack_top - user_sp_aligned).align_up_4k(),
        MappingFlags::READ | MappingFlags::WRITE,
    )?;
    uspace.write(user_sp, stack_data.as_slice())?;

    let heap_start = VirtAddr::from_usize(kaddr_layout::USER_HEAP_BASE);
    let heap_size = kaddr_layout::USER_HEAP_SIZE;
    uspace.map(
        heap_start,
        heap_size,
        MappingFlags::READ | MappingFlags::WRITE | MappingFlags::USER,
        true,
        VmRuntimeRef::new_anon_private(heap_start, PageSize::Size4K),
    )?;

    Ok((entry, user_sp))
}

#[cfg(unittest)]
mod tests {
    use alloc::{borrow::ToOwned, vec};

    use khal::paging::MappingFlags;
    use unittest::def_test;
    use xmas_elf::program::{FLAG_R, FLAG_W, FLAG_X, Flags};

    use super::{ExecRequest, ExecSource, mapping_flags, script_interpreter_args};

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
        let request = ExecRequest::from_path("/bin/app", args.clone(), envs.clone());

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
            script_interpreter_args("/bin/sh -e", "/tmp/script.sh", original.as_slice());

        assert_eq!(rewritten.len(), 5);
        assert_eq!(rewritten[0], "/bin/sh");
        assert_eq!(rewritten[1], "-e");
        assert_eq!(rewritten[2], "/tmp/script.sh");
        assert_eq!(rewritten[3], "arg1");
        assert_eq!(rewritten[4], "arg2");
    }
}
