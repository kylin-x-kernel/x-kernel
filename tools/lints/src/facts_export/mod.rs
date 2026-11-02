// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `facts_export`: writes a per-crate code-facts file (items, MIR call edges,
//! functions used as values, trait impls, kiface symbols, atomic and unsafe
//! sites) for the review code index.
//!
//! The pass emits no diagnostics and does nothing unless the environment
//! variable [`ENABLE_ENV`] is `1`. Then each crate the driver checks writes
//! `<stem>.facts.jsonl` next to its metadata (`<out-dir>/lib<stem>.rmeta`),
//! where `<stem>` is the crate name plus Cargo's `-C extra-filename`. A crate
//! that Cargo finds fresh keeps the file from the run that checked it, and
//! a crate whose configuration changed gets a new stem, so the set of files
//! for one build is exactly the set of units in it. `xkmake dylint
//! --facts-out` selects that set and packs it.
//!
//! The variable is a tracked environment dependency: switching it on or off
//! re-checks the workspace crates, while keeping it at a fixed value lets
//! Cargo reuse every fresh crate together with its facts file.
//!
//! Call edges come from MIR after drop elaboration and before optimisation,
//! so macro output, operator overloading, `?`, `for` and drops are included
//! and no call is inlined away. When that MIR was already consumed (bodies
//! that const evaluation or coroutine layout optimised early), `const fn`
//! bodies use their CTFE MIR, which is never inlined, and other bodies use
//! optimised MIR with the inlined calls recovered from its inlined source
//! scopes.
//!
//! The record format is described in `tools/lints/README.md`; its version
//! is [`SCHEMA`].

mod bodies;
mod items;
mod json;
mod paths;

use std::{
    collections::HashMap,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
};

use rustc_hir::def_id::{DefId, LOCAL_CRATE, LocalDefId};
use rustc_lint::{LateContext, LateLintPass, LintContext};
use rustc_middle::ty::{self, Ty, TyCtxt};
use rustc_span::{ExpnKind, FileName, Span, Symbol};

use self::json::Record;

/// Version of the facts record format.
pub(crate) const SCHEMA: &str = "xkernel-facts/0.2";
/// Enables the export when set to `1`.
pub(crate) const ENABLE_ENV: &str = "XKERNEL_FACTS";
/// Extension of a crate's facts file, after its stem.
const FILE_EXTENSION: &str = "facts.jsonl";

rustc_session::declare_lint! {
    /// ### What it does
    ///
    /// Never reports. When `XKERNEL_FACTS=1`, writes the crate's code facts
    /// for the review code index; see the module documentation.
    ///
    /// ### Why is this bad?
    ///
    /// It is not a rule; the lint only gives the export pass a name. It is
    /// `Warn` because rustc skips late passes whose lints are all allowed.
    pub FACTS_EXPORT,
    Warn,
    "export code facts for the review code index"
}

rustc_session::declare_lint_pass!(FactsExport => [FACTS_EXPORT]);

/// Registers [`ENABLE_ENV`] as a tracked environment variable, so that Cargo
/// re-checks a crate when the export is switched on or off.
pub(crate) fn track_env(sess: &rustc_session::Session) {
    let value = std::env::var(ENABLE_ENV).ok().map(|v| Symbol::intern(&v));
    sess.psess
        .env_depinfo
        .lock()
        .insert((Symbol::intern(ENABLE_ENV), value));
}

fn enabled() -> bool {
    std::env::var_os(ENABLE_ENV).is_some_and(|value| value == "1")
}

impl<'tcx> LateLintPass<'tcx> for FactsExport {
    fn check_crate_post(&mut self, cx: &LateContext<'tcx>) {
        if !enabled() {
            return;
        }
        let tcx = cx.tcx;
        let crate_name = tcx.crate_name(LOCAL_CRATE);
        // Build scripts and autocfg probes are host programs, not kernel code.
        if crate_name.as_str().starts_with("build_script_")
            || crate_name.as_str().starts_with("autocfg_")
        {
            return;
        }
        let started = std::time::Instant::now();
        // `<out-dir>/<stem>.facts.jsonl`; the stem has no dot of its own.
        let path = tcx.output_filenames(()).with_extension(FILE_EXTENSION);
        let stem = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(FILE_EXTENSION))
            .and_then(|name| name.strip_suffix('.'))
            .unwrap_or_default()
            .to_owned();
        let root = std::env::current_dir().unwrap_or_default();
        let mut exporter = Exporter::new(tcx, root);
        exporter.header(&stem);
        exporter.items();
        exporter.bodies();
        let elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        exporter.counts.insert("export_us", elapsed_us);
        let bytes = exporter.buf.len() as u64;
        exporter.counts.insert("bytes", bytes);
        exporter.footer();
        if let Err(error) = write_atomic(&path, exporter.buf.as_bytes()) {
            cx.sess().dcx().warn(format!(
                "facts_export: cannot write {}: {error}",
                path.display()
            ));
        }
    }
}

/// Writes through a temporary file so that a reader never sees a partial
/// facts file.
fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(".{name}.{}", std::process::id()));
    let mut file = fs::File::create(&tmp)?;
    file.write_all(data)?;
    drop(file);
    fs::rename(tmp, path)
}

/// Where a span came from: file, line, column, and the macro that produced
/// it (outermost user-visible invocation), if any.
struct Place {
    file: String,
    line: u64,
    col: u64,
    end_line: u64,
    /// `name!`, `#[name]` or `#[derive(Name)]` of the outermost macro.
    mac: Option<String>,
    /// Innermost macro, when different from the outermost one.
    inner_mac: Option<String>,
    /// Compiler desugaring (`?`, `for`, `async`/`await`, ...), if any.
    desugar: Option<String>,
}

struct Exporter<'tcx> {
    tcx: TyCtxt<'tcx>,
    root: PathBuf,
    buf: String,
    counts: HashMap<&'static str, u64>,
    /// User-written `unsafe` blocks by typeck root (call-site spans), for
    /// the `in_unsafe` flag of calls.
    unsafe_blocks: HashMap<LocalDefId, Vec<Span>>,
}

impl<'tcx> Exporter<'tcx> {
    fn new(tcx: TyCtxt<'tcx>, root: PathBuf) -> Self {
        Self {
            tcx,
            root,
            buf: String::with_capacity(1 << 20),
            counts: HashMap::new(),
            unsafe_blocks: HashMap::new(),
        }
    }

    fn bump(&mut self, key: &'static str) {
        *self.counts.entry(key).or_default() += 1;
    }

    fn header(&mut self, stem: &str) {
        let tcx = self.tcx;
        let sess = tcx.sess;
        let mut cfgs: Vec<String> = sess
            .psess
            .config
            .iter()
            .filter(|(name, _)| {
                matches!(
                    name.as_str(),
                    "feature" | "unittest" | "test" | "debug_assertions"
                )
            })
            .map(|(name, value)| match value {
                Some(v) => format!("{name}=\"{v}\""),
                None => name.to_string(),
            })
            .collect();
        cfgs.sort();
        let target = sess.opts.target_triple.tuple().to_owned();
        let toolchain = std::env::var("RUSTUP_TOOLCHAIN").unwrap_or_default();
        // Cargo passes the package identity to every rustc it runs.
        let package = std::env::var("CARGO_PKG_NAME").ok();
        let version = std::env::var("CARGO_PKG_VERSION").ok();
        let crate_name = tcx.crate_name(LOCAL_CRATE).to_string();
        let types: Vec<String> = tcx.crate_types().iter().map(|t| format!("{t:?}")).collect();
        Record::new(&mut self.buf, "crate")
            .str("schema", SCHEMA)
            .str("crate", &crate_name)
            .str(
                "crate_id",
                &format!("{:016x}", tcx.stable_crate_id(LOCAL_CRATE).as_u64()),
            )
            .str("stem", stem)
            .opt("package", package.as_deref())
            .opt("version", version.as_deref())
            .str("kind", "workspace")
            .str("target", &target)
            .str("toolchain", &toolchain)
            .str(
                "rustc",
                rustc_interface::util::rustc_version_str().unwrap_or(""),
            )
            .str("crate_types", &types.join(","))
            .str("opt_level", &format!("{:?}", sess.opts.optimize))
            .raw("cfg", &json::str_array(cfgs.iter().map(String::as_str)))
            .end();
    }

    fn footer(&mut self) {
        let mut counts: Vec<_> = self.counts.iter().map(|(k, v)| (*k, *v)).collect();
        counts.sort_unstable();
        let mut obj = String::from("{");
        for (index, (key, value)) in counts.iter().enumerate() {
            if index > 0 {
                obj.push(',');
            }
            json::push_str(&mut obj, key);
            obj.push(':');
            obj.push_str(&value.to_string());
        }
        obj.push('}');
        Record::new(&mut self.buf, "counts")
            .raw("counts", &obj)
            .end();
    }

    fn file_of(&self, name: &FileName) -> String {
        match name {
            FileName::Real(real) => match real.local_path() {
                Some(path) => paths::display(path, &self.root),
                None => paths::display_str(&name.prefer_local_unconditionally().to_string()),
            },
            other => paths::display_str(&other.prefer_local_unconditionally().to_string()),
        }
    }

    fn place(&self, span: Span) -> Place {
        let sm = self.tcx.sess.source_map();
        let mut outer = None;
        let mut inner = None;
        let mut desugar = None;
        for data in span.macro_backtrace() {
            match data.kind {
                ExpnKind::Macro(kind, name) => {
                    let text = ExpnKind::Macro(kind, name).descr();
                    if inner.is_none() {
                        inner = Some(text.clone());
                    }
                    outer = Some(text);
                }
                ExpnKind::Desugaring(kind) => {
                    if desugar.is_none() {
                        desugar = Some(format!("{kind:?}"));
                    }
                }
                ExpnKind::AstPass(_) | ExpnKind::Root => {}
            }
        }
        // `macro_backtrace` stops at desugarings' call sites too; record the
        // innermost desugaring even when no macro is involved.
        if desugar.is_none()
            && span.from_expansion()
            && let ExpnKind::Desugaring(kind) = span.ctxt().outer_expn_data().kind
        {
            desugar = Some(format!("{kind:?}"));
        }
        let site = span.source_callsite();
        let lo = sm.lookup_char_pos(site.lo());
        let hi = sm.lookup_char_pos(site.hi());
        let inner_mac = if inner != outer { inner } else { None };
        Place {
            file: self.file_of(&lo.file.name),
            line: lo.line as u64,
            col: lo.col.0 as u64 + 1,
            end_line: hi.line as u64,
            mac: outer,
            inner_mac,
            desugar,
        }
    }

    fn put_place<'a>(record: Record<'a>, place: &Place) -> Record<'a> {
        record
            .str("file", &place.file)
            .num("line", place.line)
            .num("col", place.col)
            .opt("macro", place.mac.as_deref())
            .opt("inner_macro", place.inner_mac.as_deref())
            .opt("desugar", place.desugar.as_deref())
    }

    /// Writes where `def_id` is defined, as `callee_file` / `callee_line`.
    fn def_place<'a>(&self, record: Record<'a>, def_id: DefId) -> Record<'a> {
        let sm = self.tcx.sess.source_map();
        let span = self.tcx.def_span(def_id).source_callsite();
        if span.is_dummy() {
            return record;
        }
        let lo = sm.lookup_char_pos(span.lo());
        record
            .str("callee_file", &self.file_of(&lo.file.name))
            .num("callee_line", lo.line as u64)
    }

    /// Key of a definition: `crate::path::{impl#N}::name`.
    fn key(&self, def_id: DefId) -> String {
        let tcx = self.tcx;
        format!(
            "{}{}",
            tcx.crate_name(def_id.krate),
            tcx.def_path(def_id).to_string_no_crate_verbose()
        )
    }

    /// Human-readable path, e.g. `<kspin::SpinLock<G, T>>::lock`.
    fn pretty(&self, def_id: DefId) -> String {
        ty::print::with_no_trimmed_paths!(self.tcx.def_path_str(def_id))
    }

    /// Last path segment of a type, for name-level joins (`Foo` for
    /// `Foo<T>`, `[T]` for slices).
    fn type_short(&self, ty: Ty<'tcx>) -> String {
        match ty.kind() {
            ty::Adt(def, _) => self.tcx.item_name(def.did()).to_string(),
            ty::Foreign(did) => self.tcx.item_name(*did).to_string(),
            ty::Dynamic(preds, ..) => preds
                .principal_def_id()
                .map(|d| format!("dyn {}", self.tcx.item_name(d)))
                .unwrap_or_else(|| "dyn".into()),
            ty::Ref(_, inner, _) | ty::RawPtr(inner, _) => self.type_short(*inner),
            _ => ty::print::with_no_trimmed_paths!(ty.to_string()),
        }
    }

    /// Short self type of the impl that owns `def_id`, if any.
    fn impl_owner(&self, def_id: DefId) -> Option<String> {
        let tcx = self.tcx;
        let parent = tcx.opt_parent(def_id)?;
        if !matches!(tcx.def_kind(parent), rustc_hir::def::DefKind::Impl { .. }) {
            return None;
        }
        Some(self.type_short(tcx.type_of(parent).instantiate_identity()))
    }
}

#[cfg(test)]
mod tests;
