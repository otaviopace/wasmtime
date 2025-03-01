//! Define the `instantiate` function, which takes a byte array containing an
//! encoded wasm module and returns a live wasm instance. Also, define
//! `CompiledModule` to allow compiling and instantiating to be done as separate
//! steps.

use crate::code_memory::CodeMemory;
use crate::compiler::{Compilation, Compiler};
use crate::debug::create_gdbjit_image;
use crate::link::link_module;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::ops::Range;
use std::sync::Arc;
use thiserror::Error;
use wasmtime_environ::entity::PrimaryMap;
use wasmtime_environ::wasm::{
    DefinedFuncIndex, InstanceTypeIndex, ModuleTypeIndex, SignatureIndex, WasmFuncType,
};
use wasmtime_environ::{
    CompileError, DebugInfoData, FunctionAddressMap, InstanceSignature, Module, ModuleEnvironment,
    ModuleSignature, ModuleTranslation, StackMapInformation, TrapInformation,
};
use wasmtime_profiling::ProfilingAgent;
use wasmtime_runtime::{GdbJitImageRegistration, InstantiationError, VMFunctionBody, VMTrampoline};

/// An error condition while setting up a wasm instance, be it validation,
/// compilation, or instantiation.
#[derive(Error, Debug)]
pub enum SetupError {
    /// The module did not pass validation.
    #[error("Validation error: {0}")]
    Validate(String),

    /// A wasm translation error occurred.
    #[error("WebAssembly failed to compile")]
    Compile(#[from] CompileError),

    /// Some runtime resource was unavailable or insufficient, or the start function
    /// trapped.
    #[error("Instantiation failed during setup")]
    Instantiate(#[from] InstantiationError),

    /// Debug information generation error occurred.
    #[error("Debug information error")]
    DebugInfo(#[from] anyhow::Error),
}

/// Contains all compilation artifacts.
#[derive(Serialize, Deserialize)]
pub struct CompilationArtifacts {
    /// Module metadata.
    #[serde(with = "arc_serde")]
    module: Arc<Module>,

    /// ELF image with functions code.
    obj: Box<[u8]>,

    /// Descriptions of compiled functions
    funcs: PrimaryMap<DefinedFuncIndex, FunctionInfo>,

    /// Whether or not native debug information is available in `obj`
    native_debug_info_present: bool,

    /// Whether or not the original wasm module contained debug information that
    /// we skipped and did not parse.
    has_unparsed_debuginfo: bool,

    /// Debug information found in the wasm file, used for symbolicating
    /// backtraces.
    debug_info: Option<DebugInfo>,
}

#[derive(Serialize, Deserialize)]
struct DebugInfo {
    data: Box<[u8]>,
    code_section_offset: u64,
    debug_abbrev: Range<usize>,
    debug_addr: Range<usize>,
    debug_aranges: Range<usize>,
    debug_info: Range<usize>,
    debug_line: Range<usize>,
    debug_line_str: Range<usize>,
    debug_ranges: Range<usize>,
    debug_rnglists: Range<usize>,
    debug_str: Range<usize>,
    debug_str_offsets: Range<usize>,
}

impl CompilationArtifacts {
    /// Creates a `CompilationArtifacts` for a singular translated wasm module.
    ///
    /// The `use_paged_init` argument controls whether or not an attempt is made to
    /// organize linear memory initialization data as entire pages or to leave
    /// the memory initialization data as individual segments.
    pub fn build(
        compiler: &Compiler,
        data: &[u8],
        use_paged_mem_init: bool,
    ) -> Result<(usize, Vec<CompilationArtifacts>, TypeTables), SetupError> {
        let (main_module, translations, types) =
            ModuleEnvironment::new(compiler.tunables(), compiler.features())
                .translate(data)
                .map_err(|error| SetupError::Compile(CompileError::Wasm(error)))?;

        let list = compiler.run_maybe_parallel::<_, _, SetupError, _>(
            translations,
            |mut translation| {
                let Compilation { obj, funcs } = compiler.compile(&mut translation, &types)?;

                let ModuleTranslation {
                    mut module,
                    debuginfo,
                    has_unparsed_debuginfo,
                    ..
                } = translation;

                if use_paged_mem_init {
                    if let Some(init) = module.memory_initialization.to_paged(&module) {
                        module.memory_initialization = init;
                    }
                }

                Ok(CompilationArtifacts {
                    module: Arc::new(module),
                    obj: obj.into_boxed_slice(),
                    funcs: funcs
                        .into_iter()
                        .map(|(_, func)| FunctionInfo {
                            stack_maps: func.stack_maps,
                            traps: func.traps,
                            address_map: func.address_map,
                        })
                        .collect(),
                    native_debug_info_present: compiler.tunables().generate_native_debuginfo,
                    debug_info: if compiler.tunables().parse_wasm_debuginfo {
                        Some(debuginfo.into())
                    } else {
                        None
                    },
                    has_unparsed_debuginfo,
                })
            },
        )?;

        Ok((
            main_module,
            list,
            TypeTables {
                wasm_signatures: types.wasm_signatures,
                module_signatures: types.module_signatures,
                instance_signatures: types.instance_signatures,
            },
        ))
    }
}

struct FinishedFunctions(PrimaryMap<DefinedFuncIndex, *mut [VMFunctionBody]>);
unsafe impl Send for FinishedFunctions {}
unsafe impl Sync for FinishedFunctions {}

/// Information about a function, such as trap information, address map,
/// and stack maps.
#[derive(Serialize, Deserialize, Clone)]
pub struct FunctionInfo {
    pub traps: Vec<TrapInformation>,
    pub address_map: FunctionAddressMap,
    pub stack_maps: Vec<StackMapInformation>,
}

/// This is intended to mirror the type tables in `wasmtime_environ`, except that
/// it doesn't store the native signatures which are no longer needed past compilation.
#[derive(Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct TypeTables {
    pub wasm_signatures: PrimaryMap<SignatureIndex, WasmFuncType>,
    pub module_signatures: PrimaryMap<ModuleTypeIndex, ModuleSignature>,
    pub instance_signatures: PrimaryMap<InstanceTypeIndex, InstanceSignature>,
}

/// Container for data needed for an Instance function to exist.
pub struct ModuleCode {
    range: (usize, usize),
    code_memory: CodeMemory,
    #[allow(dead_code)]
    dbg_jit_registration: Option<GdbJitImageRegistration>,
}

impl ModuleCode {
    /// Gets the [begin, end) range of the module's code.
    pub fn range(&self) -> (usize, usize) {
        self.range
    }
}

/// A compiled wasm module, ready to be instantiated.
pub struct CompiledModule {
    artifacts: CompilationArtifacts,
    code: Arc<ModuleCode>,
    finished_functions: FinishedFunctions,
    trampolines: Vec<(SignatureIndex, VMTrampoline)>,
}

impl CompiledModule {
    /// Creates a list of compiled modules from the given list of compilation
    /// artifacts.
    pub fn from_artifacts_list(
        artifacts: Vec<CompilationArtifacts>,
        profiler: &dyn ProfilingAgent,
        compiler: &Compiler,
    ) -> Result<Vec<Arc<Self>>, SetupError> {
        compiler.run_maybe_parallel(artifacts, |a| CompiledModule::from_artifacts(a, profiler))
    }

    /// Creates `CompiledModule` directly from `CompilationArtifacts`.
    pub fn from_artifacts(
        artifacts: CompilationArtifacts,
        profiler: &dyn ProfilingAgent,
    ) -> Result<Arc<Self>, SetupError> {
        // Allocate all of the compiled functions into executable memory,
        // copying over their contents.
        let (code_memory, code_range, finished_functions, trampolines) =
            build_code_memory(&artifacts.obj, &artifacts.module).map_err(|message| {
                SetupError::Instantiate(InstantiationError::Resource(anyhow::anyhow!(
                    "failed to build code memory for functions: {}",
                    message
                )))
            })?;

        // Register GDB JIT images; initialize profiler and load the wasm module.
        let dbg_jit_registration = if artifacts.native_debug_info_present {
            let bytes = create_dbg_image(
                artifacts.obj.to_vec(),
                code_range,
                &artifacts.module,
                &finished_functions,
            )?;
            profiler.module_load(&artifacts.module, &finished_functions, Some(&bytes));
            let reg = GdbJitImageRegistration::register(bytes);
            Some(reg)
        } else {
            profiler.module_load(&artifacts.module, &finished_functions, None);
            None
        };

        let finished_functions = FinishedFunctions(finished_functions);
        let start = code_range.0 as usize;
        let end = start + code_range.1;

        Ok(Arc::new(Self {
            artifacts,
            code: Arc::new(ModuleCode {
                range: (start, end),
                code_memory,
                dbg_jit_registration,
            }),
            finished_functions,
            trampolines,
        }))
    }

    /// Extracts `CompilationArtifacts` from the compiled module.
    pub fn compilation_artifacts(&self) -> &CompilationArtifacts {
        &self.artifacts
    }

    /// Return a reference-counting pointer to a module.
    pub fn module(&self) -> &Arc<Module> {
        &self.artifacts.module
    }

    /// Return a reference to a mutable module (if possible).
    pub fn module_mut(&mut self) -> Option<&mut Module> {
        Arc::get_mut(&mut self.artifacts.module)
    }

    /// Returns the map of all finished JIT functions compiled for this module
    #[inline]
    pub fn finished_functions(&self) -> &PrimaryMap<DefinedFuncIndex, *mut [VMFunctionBody]> {
        &self.finished_functions.0
    }

    /// Returns the per-signature trampolines for this module.
    pub fn trampolines(&self) -> &[(SignatureIndex, VMTrampoline)] {
        &self.trampolines
    }

    /// Returns the stack map information for all functions defined in this
    /// module.
    ///
    /// The iterator returned iterates over the span of the compiled function in
    /// memory with the stack maps associated with those bytes.
    pub fn stack_maps(
        &self,
    ) -> impl Iterator<Item = (*mut [VMFunctionBody], &[StackMapInformation])> {
        self.finished_functions().values().copied().zip(
            self.artifacts
                .funcs
                .values()
                .map(|f| f.stack_maps.as_slice()),
        )
    }

    /// Lookups a defined function by a program counter value.
    ///
    /// Returns the defined function index, the start address, and the end address (exclusive).
    pub fn func_by_pc(&self, pc: usize) -> Option<(DefinedFuncIndex, usize, usize)> {
        let functions = self.finished_functions();

        let index = match functions.binary_search_values_by_key(&pc, |body| unsafe {
            debug_assert!(!(**body).is_empty());
            // Return the inclusive "end" of the function
            (**body).as_ptr() as usize + (**body).len() - 1
        }) {
            Ok(k) => {
                // Exact match, pc is at the end of this function
                k
            }
            Err(k) => {
                // Not an exact match, k is where `pc` would be "inserted"
                // Since we key based on the end, function `k` might contain `pc`,
                // so we'll validate on the range check below
                k
            }
        };

        let body = functions.get(index)?;
        let (start, end) = unsafe {
            let ptr = (**body).as_ptr();
            let len = (**body).len();
            (ptr as usize, ptr as usize + len)
        };

        if pc < start || end < pc {
            return None;
        }

        Some((index, start, end))
    }

    /// Gets the function information for a given function index.
    pub fn func_info(&self, index: DefinedFuncIndex) -> &FunctionInfo {
        self.artifacts
            .funcs
            .get(index)
            .expect("defined function should be present")
    }

    /// Returns all ranges covered by JIT code.
    pub fn jit_code_ranges<'a>(&'a self) -> impl Iterator<Item = (usize, usize)> + 'a {
        self.code.code_memory.published_ranges()
    }

    /// Returns module's JIT code.
    pub fn code(&self) -> &Arc<ModuleCode> {
        &self.code
    }

    /// Creates a new symbolication context which can be used to further
    /// symbolicate stack traces.
    ///
    /// Basically this makes a thing which parses debuginfo and can tell you
    /// what filename and line number a wasm pc comes from.
    pub fn symbolize_context(&self) -> Result<Option<SymbolizeContext>, gimli::Error> {
        use gimli::EndianSlice;
        let info = match &self.artifacts.debug_info {
            Some(info) => info,
            None => return Ok(None),
        };
        // For now we clone the data into the `SymbolizeContext`, but if this
        // becomes prohibitive we could always `Arc` it with our own allocation
        // here.
        let data = info.data.clone();
        let endian = gimli::LittleEndian;
        let cx = addr2line::Context::from_sections(
            EndianSlice::new(&data[info.debug_abbrev.clone()], endian).into(),
            EndianSlice::new(&data[info.debug_addr.clone()], endian).into(),
            EndianSlice::new(&data[info.debug_aranges.clone()], endian).into(),
            EndianSlice::new(&data[info.debug_info.clone()], endian).into(),
            EndianSlice::new(&data[info.debug_line.clone()], endian).into(),
            EndianSlice::new(&data[info.debug_line_str.clone()], endian).into(),
            EndianSlice::new(&data[info.debug_ranges.clone()], endian).into(),
            EndianSlice::new(&data[info.debug_rnglists.clone()], endian).into(),
            EndianSlice::new(&data[info.debug_str.clone()], endian).into(),
            EndianSlice::new(&data[info.debug_str_offsets.clone()], endian).into(),
            EndianSlice::new(&[], endian),
        )?;
        Ok(Some(SymbolizeContext {
            // See comments on `SymbolizeContext` for why we do this static
            // lifetime promotion.
            inner: unsafe {
                std::mem::transmute::<Addr2LineContext<'_>, Addr2LineContext<'static>>(cx)
            },
            code_section_offset: info.code_section_offset,
            _data: data,
        }))
    }

    /// Returns whether the original wasm module had unparsed debug information
    /// based on the tunables configuration.
    pub fn has_unparsed_debuginfo(&self) -> bool {
        self.artifacts.has_unparsed_debuginfo
    }
}

type Addr2LineContext<'a> = addr2line::Context<gimli::EndianSlice<'a, gimli::LittleEndian>>;

/// A context which contains dwarf debug information to translate program
/// counters back to filenames and line numbers.
pub struct SymbolizeContext {
    // Note the `'static` lifetime on `inner`. That's actually a bunch of slices
    // which point back into the `_data` field. We currently unsafely manage
    // this by saying that when inside the struct it's `'static` (since we own
    // the referenced data just next to it) and we only loan out borrowed
    // references.
    _data: Box<[u8]>,
    inner: Addr2LineContext<'static>,
    code_section_offset: u64,
}

impl SymbolizeContext {
    /// Returns access to the [`addr2line::Context`] which can be used to query
    /// frame information with.
    pub fn addr2line(&self) -> &Addr2LineContext<'_> {
        // Here we demote our synthetic `'static` lifetime which doesn't
        // actually exist back to a lifetime that's tied to `&self`, which
        // should be safe.
        unsafe {
            std::mem::transmute::<&Addr2LineContext<'static>, &Addr2LineContext<'_>>(&self.inner)
        }
    }

    /// Returns the offset of the code section in the original wasm file, used
    /// to calculate lookup values into the DWARF.
    pub fn code_section_offset(&self) -> u64 {
        self.code_section_offset
    }
}

fn create_dbg_image(
    obj: Vec<u8>,
    code_range: (*const u8, usize),
    module: &Module,
    finished_functions: &PrimaryMap<DefinedFuncIndex, *mut [VMFunctionBody]>,
) -> Result<Vec<u8>, SetupError> {
    let funcs = finished_functions
        .values()
        .map(|allocated: &*mut [VMFunctionBody]| (*allocated) as *const u8)
        .collect::<Vec<_>>();
    create_gdbjit_image(obj, code_range, module.num_imported_funcs, &funcs)
        .map_err(SetupError::DebugInfo)
}

fn build_code_memory(
    obj: &[u8],
    module: &Module,
) -> Result<(
    CodeMemory,
    (*const u8, usize),
    PrimaryMap<DefinedFuncIndex, *mut [VMFunctionBody]>,
    Vec<(SignatureIndex, VMTrampoline)>,
)> {
    let mut code_memory = CodeMemory::new();

    let allocation = code_memory.allocate_for_object(obj)?;

    // Populate the finished functions from the allocation
    let mut finished_functions = PrimaryMap::with_capacity(allocation.funcs_len());
    for (i, fat_ptr) in allocation.funcs() {
        let start = fat_ptr.as_ptr() as usize;
        let fat_ptr: *mut [VMFunctionBody] = fat_ptr;
        // Assert that the function bodies are pushed in sort order
        // This property is relied upon to search for functions by PC values
        assert!(
            start
                > finished_functions
                    .last()
                    .map(|f: &*mut [VMFunctionBody]| unsafe { (**f).as_ptr() as usize })
                    .unwrap_or(0)
        );
        assert_eq!(
            Some(finished_functions.push(fat_ptr)),
            module.defined_func_index(i)
        );
    }

    // Populate the trampolines from the allocation
    let mut trampolines = Vec::with_capacity(allocation.trampolines_len());
    for (i, fat_ptr) in allocation.trampolines() {
        let fnptr =
            unsafe { std::mem::transmute::<*const VMFunctionBody, VMTrampoline>(fat_ptr.as_ptr()) };
        trampolines.push((i, fnptr));
    }

    link_module(
        &allocation.obj,
        &module,
        allocation.code_range,
        &finished_functions,
    );

    let code_range = (allocation.code_range.as_ptr(), allocation.code_range.len());

    // Make all code compiled thus far executable.
    code_memory.publish();

    Ok((code_memory, code_range, finished_functions, trampolines))
}

impl From<DebugInfoData<'_>> for DebugInfo {
    fn from(raw: DebugInfoData<'_>) -> DebugInfo {
        use gimli::Section;

        let mut data = Vec::new();
        let mut push = |section: &[u8]| {
            data.extend_from_slice(section);
            data.len() - section.len()..data.len()
        };
        let debug_abbrev = push(raw.dwarf.debug_abbrev.reader().slice());
        let debug_addr = push(raw.dwarf.debug_addr.reader().slice());
        let debug_aranges = push(raw.dwarf.debug_aranges.reader().slice());
        let debug_info = push(raw.dwarf.debug_info.reader().slice());
        let debug_line = push(raw.dwarf.debug_line.reader().slice());
        let debug_line_str = push(raw.dwarf.debug_line_str.reader().slice());
        let debug_ranges = push(raw.debug_ranges.reader().slice());
        let debug_rnglists = push(raw.debug_rnglists.reader().slice());
        let debug_str = push(raw.dwarf.debug_str.reader().slice());
        let debug_str_offsets = push(raw.dwarf.debug_str_offsets.reader().slice());
        DebugInfo {
            data: data.into(),
            debug_abbrev,
            debug_addr,
            debug_aranges,
            debug_info,
            debug_line,
            debug_line_str,
            debug_ranges,
            debug_rnglists,
            debug_str,
            debug_str_offsets,
            code_section_offset: raw.wasm_file.code_section_offset,
        }
    }
}

mod arc_serde {
    use super::Arc;
    use serde::{de::Deserialize, ser::Serialize, Deserializer, Serializer};

    pub(super) fn serialize<S, T>(arc: &Arc<T>, ser: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: Serialize,
    {
        (**arc).serialize(ser)
    }

    pub(super) fn deserialize<'de, D, T>(de: D) -> Result<Arc<T>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        Ok(Arc::new(T::deserialize(de)?))
    }
}
