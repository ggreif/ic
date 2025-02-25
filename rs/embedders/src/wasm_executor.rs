use std::path::PathBuf;
use std::sync::Arc;

use ic_replicated_state::canister_state::execution_state::WasmBinary;
use ic_replicated_state::{ExportedFunctions, Global, Memory, NumWasmPages, PageMap};
use ic_system_api::sandbox_safe_system_state::{SandboxSafeSystemState, SystemStateChanges};
use ic_system_api::{ApiType, DefaultOutOfInstructionsHandler};
use ic_types::methods::{FuncRef, WasmMethod};
use prometheus::IntCounter;
use serde::{Deserialize, Serialize};
use wasmtime::Module;

use crate::{
    wasm_utils::{compile, decoding::decode_wasm, Segments, WasmImportsDetails},
    wasmtime_embedder::WasmtimeInstance,
    CompilationCache, CompilationResult, SerializedModule, WasmExecutionInput, WasmtimeEmbedder,
};
use ic_config::flag_status::FlagStatus;
use ic_interfaces::execution_environment::{
    HypervisorError, HypervisorResult, InstanceStats, OutOfInstructionsHandler,
    SubnetAvailableMemory, SystemApi, WasmExecutionOutput,
};
use ic_logger::{warn, ReplicaLogger};
use ic_metrics::MetricsRegistry;
use ic_replicated_state::{EmbedderCache, ExecutionState};
use ic_sys::{page_bytes_from_ptr, PageBytes, PageIndex, PAGE_SIZE};
use ic_system_api::{
    system_api_empty::SystemApiEmpty, ExecutionParameters, ModificationTracking, SystemApiImpl,
};
use ic_types::{CanisterId, NumBytes, NumInstructions};
use ic_wasm_types::{BinaryEncodedWasm, CanisterModule};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

// Please enable only for debugging.
// If enabled, will collect and log checksums of execution results.
// Disabled by default to avoid producing too much data.
const EMIT_STATE_HASHES_FOR_DEBUGGING: FlagStatus = FlagStatus::Disabled;

/// The interface of a WebAssembly execution engine.
/// Currently it has two implementations:
/// - `SandboxedExecutionController` for out-of-process sandboxed execution.
/// - `WasmExecutorImpl` for in-process execution.
pub trait WasmExecutor: Send + Sync {
    fn execute(
        self: Arc<Self>,
        input: WasmExecutionInput,
        execution_state: &ExecutionState,
    ) -> (Option<CompilationResult>, WasmExecutionResult);

    fn create_execution_state(
        &self,
        canister_module: CanisterModule,
        canister_root: PathBuf,
        canister_id: CanisterId,
        compilation_cache: Arc<CompilationCache>,
    ) -> HypervisorResult<(ExecutionState, NumInstructions, Option<CompilationResult>)>;
}

struct WasmExecutorMetrics {
    // TODO(EXC-365): Remove these metrics once we confirm that no module imports these IC0 methods
    // anymore.
    imports_call_simple: IntCounter,
    imports_controller_size: IntCounter,
    imports_controller_copy: IntCounter,
    // TODO(EXC-376): Remove these metrics once we confirm that no module imports these IC0 methods
    // anymore.
    imports_call_cycles_add: IntCounter,
    imports_canister_cycle_balance: IntCounter,
    imports_msg_cycles_available: IntCounter,
    imports_msg_cycles_refunded: IntCounter,
    imports_msg_cycles_accept: IntCounter,
    imports_mint_cycles: IntCounter,
}

impl WasmExecutorMetrics {
    #[doc(hidden)] // pub for usage in tests
    pub fn new(metrics_registry: &MetricsRegistry) -> Self {
        Self {
            imports_call_simple: metrics_registry.int_counter(
                "execution_wasm_imports_call_simple_total",
                "The number of Wasm modules that import ic0.call_simple",
            ),
            imports_controller_size: metrics_registry.int_counter(
                "execution_wasm_imports_controller_size_total",
                "The number of Wasm modules that import ic0.controller_size",
            ),
            imports_controller_copy: metrics_registry.int_counter(
                "execution_wasm_imports_controller_copy_total",
                "The number of Wasm modules that import ic0.controller_copy",
            ),
            imports_call_cycles_add: metrics_registry.int_counter(
                "execution_wasm_imports_call_cycles_add",
                "The number of Wasm modules that import ic0.call_cycles_add",
            ),
            imports_canister_cycle_balance: metrics_registry.int_counter(
                "execution_wasm_imports_canister_cycle_balance",
                "The number of Wasm modules that import ic0.canister_cycle_balance",
            ),
            imports_msg_cycles_available: metrics_registry.int_counter(
                "execution_wasm_imports_msg_cycles_available",
                "The number of Wasm modules that import ic0.msg_cycles_available",
            ),
            imports_msg_cycles_refunded: metrics_registry.int_counter(
                "execution_wasm_imports_msg_cycles_refunded",
                "The number of Wasm modules that import ic0.msg_cycles_refunded",
            ),
            imports_msg_cycles_accept: metrics_registry.int_counter(
                "execution_wasm_imports_msg_cycles_accept",
                "The number of Wasm modules that import ic0.msg_cycles_accept",
            ),
            imports_mint_cycles: metrics_registry.int_counter(
                "execution_wasm_imports_mint_cycles",
                "The number of Wasm modules that import ic0.mint_cycles",
            ),
        }
    }
}

/// Contains information about execution of the current slice.
#[derive(Serialize, Deserialize, Clone)]
pub struct SliceExecutionOutput {
    /// The number of instructions executed by the slice.
    pub executed_instructions: NumInstructions,
}

/// Represents a paused WebAssembly execution that can be resumed or aborted.
pub trait PausedWasmExecution: std::fmt::Debug + Send {
    /// Resumes the paused execution.
    /// It takes the execution state before this execution has started and
    /// the current subnet available memory.
    /// If the execution finishes, then returns the result and the state changes
    /// of the execution.
    /// Otherwise, returns an opaque object representing the paused execution.
    fn resume(self: Box<Self>, execution_state: &ExecutionState) -> WasmExecutionResult;

    /// Aborts the paused execution.
    fn abort(self: Box<Self>);
}

/// Changes in the canister state after a successul Wasm execution.
#[derive(Clone, Debug)]
pub struct CanisterStateChanges {
    /// The state of the global variables after execution.
    pub globals: Vec<Global>,

    /// The state of the Wasm memory after execution.
    pub wasm_memory: Memory,

    /// The state of the stable memory after execution.
    pub stable_memory: Memory,

    pub system_state_changes: SystemStateChanges,
}

/// The result of WebAssembly execution with deterministic time slicing.
/// If the execution is finished, then it contains the result of the execution
/// and the delta of state changes.
/// Otherwise, it contains an opaque object representing the paused execution.
#[allow(clippy::large_enum_variant)]
pub enum WasmExecutionResult {
    Finished(
        SliceExecutionOutput,
        WasmExecutionOutput,
        Option<CanisterStateChanges>,
    ),
    Paused(SliceExecutionOutput, Box<dyn PausedWasmExecution>),
}

/// An executor that can process any message (query or not) in the current
/// process. Currently this is only used for testing/debugging purposes while
/// production systems do out-of-process execution using the sandboxed
/// implementation.
pub struct WasmExecutorImpl {
    wasm_embedder: WasmtimeEmbedder,
    metrics: WasmExecutorMetrics,
    log: ReplicaLogger,
}

impl WasmExecutor for WasmExecutorImpl {
    fn execute(
        self: Arc<Self>,
        WasmExecutionInput {
            api_type,
            sandbox_safe_system_state,
            canister_current_memory_usage,
            execution_parameters,
            subnet_available_memory,
            func_ref,
            compilation_cache,
        }: WasmExecutionInput,
        execution_state: &ExecutionState,
    ) -> (Option<CompilationResult>, WasmExecutionResult) {
        // This function is called when canister sandboxing is disabled.
        // Since deterministic time slicing works only with sandboxing,
        // it must also be disabled and the execution limits must match.
        assert_eq!(
            execution_parameters.instruction_limits.message(),
            execution_parameters.instruction_limits.slice(),
        );

        // Ensure that Wasm is compiled.
        let CacheLookup {
            cache: embedder_cache,
            serialized_module,
            compilation_result,
        } = match self.get_embedder_cache(&execution_state.wasm_binary, compilation_cache) {
            Ok(cache_result) => cache_result,
            Err(err) => {
                return (
                    None,
                    wasm_execution_error(err, execution_parameters.instruction_limits.message()),
                );
            }
        };

        if let Some(serialized_module) = serialized_module {
            self.observe_metrics(&serialized_module.imports_details);
        }

        let wasm_reserved_pages = get_wasm_reserved_pages(execution_state);
        let mut wasm_memory = execution_state.wasm_memory.clone();
        let mut stable_memory = execution_state.stable_memory.clone();

        let (
            slice_execution_output,
            wasm_execution_output,
            wasm_state_changes,
            instance_or_system_api,
        ) = process(
            func_ref,
            api_type,
            canister_current_memory_usage,
            execution_parameters,
            subnet_available_memory,
            sandbox_safe_system_state,
            &embedder_cache,
            &self.wasm_embedder,
            &mut wasm_memory,
            &mut stable_memory,
            &execution_state.exported_globals,
            self.log.clone(),
            wasm_reserved_pages,
            Arc::new(DefaultOutOfInstructionsHandler {}),
        );

        // Collect logs only when the flag is enabled to avoid producing too much data.
        if EMIT_STATE_HASHES_FOR_DEBUGGING == FlagStatus::Enabled {
            self.emit_state_hashes_for_debugging(&wasm_state_changes, &wasm_execution_output);
        }

        let canister_state_changes = match wasm_state_changes {
            Some(wasm_state_changes) => {
                let system_api = match instance_or_system_api {
                    Ok(instance) => instance.into_store_data().system_api,
                    Err(system_api) => system_api,
                };
                let system_state_changes = system_api.into_system_state_changes();
                Some(CanisterStateChanges {
                    globals: wasm_state_changes.globals,
                    wasm_memory,
                    stable_memory,
                    system_state_changes,
                })
            }
            None => None,
        };

        (
            compilation_result,
            WasmExecutionResult::Finished(
                slice_execution_output,
                wasm_execution_output,
                canister_state_changes,
            ),
        )
    }

    fn create_execution_state(
        &self,
        canister_module: CanisterModule,
        canister_root: PathBuf,
        canister_id: CanisterId,
        compilation_cache: Arc<CompilationCache>,
    ) -> HypervisorResult<(ExecutionState, NumInstructions, Option<CompilationResult>)> {
        // Compile Wasm binary and cache it.
        let wasm_binary = WasmBinary::new(canister_module);
        let (embedder_cache, serialized_module, compilation_result) =
            match self.get_embedder_cache(&wasm_binary, compilation_cache)? {
                CacheLookup {
                    cache,
                    serialized_module: Some(serialized_module),
                    compilation_result,
                } => (cache, serialized_module, compilation_result),
                _ => panic!("Newly created WasmBinary must be compiled or deserialized."),
            };
        self.observe_metrics(&serialized_module.imports_details);
        let exported_functions = serialized_module.exported_functions.clone();
        let wasm_metadata = serialized_module.wasm_metadata.clone();
        let mut wasm_page_map = PageMap::default();

        let (globals, _wasm_page_delta, wasm_memory_size) = get_initial_globals_and_memory(
            &serialized_module.data_segments,
            &embedder_cache,
            &self.wasm_embedder,
            &mut wasm_page_map,
            canister_id,
        )?;

        // Create the execution state.
        let stable_memory = Memory::default();
        let execution_state = ExecutionState::new(
            canister_root,
            wasm_binary,
            ExportedFunctions::new(exported_functions),
            Memory::new(wasm_page_map, wasm_memory_size),
            stable_memory,
            globals,
            wasm_metadata,
        );
        Ok((
            execution_state,
            serialized_module.compilation_cost,
            compilation_result,
        ))
    }
}

/// Result of checking for a compiled module in the `EmbedderCache` and `CompilationCache`.
struct CacheLookup {
    pub cache: EmbedderCache,
    /// This field will be `None` if the `EmbedderCache` was present (so no module deserialization was required).
    pub serialized_module: Option<Arc<SerializedModule>>,
    /// This field will be `None` if the `SerializedModule` was present in the `CompilationCache` (so no compilation was required).
    pub compilation_result: Option<CompilationResult>,
}

impl WasmExecutorImpl {
    pub fn new(
        wasm_embedder: WasmtimeEmbedder,
        metrics_registry: &MetricsRegistry,
        log: ReplicaLogger,
    ) -> Self {
        Self {
            wasm_embedder,
            metrics: WasmExecutorMetrics::new(metrics_registry),
            log,
        }
    }

    pub fn observe_metrics(&self, imports_details: &WasmImportsDetails) {
        if imports_details.imports_call_simple {
            self.metrics.imports_call_simple.inc();
        }
        if imports_details.imports_controller_size {
            self.metrics.imports_controller_size.inc();
        }
        if imports_details.imports_controller_copy {
            self.metrics.imports_controller_copy.inc();
        }
        if imports_details.imports_call_cycles_add {
            self.metrics.imports_call_cycles_add.inc();
        }
        if imports_details.imports_canister_cycle_balance {
            self.metrics.imports_canister_cycle_balance.inc();
        }
        if imports_details.imports_msg_cycles_available {
            self.metrics.imports_msg_cycles_available.inc();
        }
        if imports_details.imports_msg_cycles_accept {
            self.metrics.imports_msg_cycles_accept.inc();
        }
        if imports_details.imports_msg_cycles_refunded {
            self.metrics.imports_msg_cycles_refunded.inc();
        }
        if imports_details.imports_mint_cycles {
            self.metrics.imports_mint_cycles.inc();
        }
    }

    fn get_embedder_cache(
        &self,
        wasm_binary: &WasmBinary,
        compilation_cache: Arc<CompilationCache>,
    ) -> HypervisorResult<CacheLookup> {
        let mut guard = wasm_binary.embedder_cache.lock().unwrap();
        if let Some(embedder_cache) = &*guard {
            Ok(CacheLookup {
                cache: embedder_cache.clone(),
                serialized_module: None,
                compilation_result: None,
            })
        } else {
            match compilation_cache.get(&wasm_binary.binary) {
                Some(Ok(serialized_module)) => {
                    let module = self
                        .wasm_embedder
                        .deserialize_module(&serialized_module.bytes);
                    let cache = EmbedderCache::new(module.clone());
                    *guard = Some(cache.clone());
                    match module {
                        Ok(_) => Ok(CacheLookup {
                            cache,
                            serialized_module: Some(serialized_module),
                            compilation_result: None,
                        }),
                        Err(err) => Err(err),
                    }
                }
                Some(Err(err)) => {
                    let cache: HypervisorResult<Module> = Err(err.clone());
                    *guard = Some(EmbedderCache::new(cache));
                    Err(err)
                }
                None => {
                    use std::borrow::Cow;
                    let decoded_wasm: Cow<'_, BinaryEncodedWasm> =
                        Cow::Owned(decode_wasm(wasm_binary.binary.to_shared_vec())?);
                    let (cache, result) = compile(&self.wasm_embedder, decoded_wasm.as_ref());
                    *guard = Some(cache.clone());
                    let (compilation_result, serialized_module) = result?;
                    let serialized_module = Arc::new(serialized_module);
                    compilation_cache
                        .insert(&wasm_binary.binary, Ok(Arc::clone(&serialized_module)));
                    Ok(CacheLookup {
                        cache,
                        serialized_module: Some(serialized_module),
                        compilation_result: Some(compilation_result),
                    })
                }
            }
        }
    }

    // Collecting information based on the result of the execution and wasm state changes.
    fn emit_state_hashes_for_debugging(
        &self,
        wasm_state_changes: &Option<WasmStateChanges>,
        wasm_execution_output: &WasmExecutionOutput,
    ) {
        // Log information only for non-empty deltas.
        // This would automatically exclude queries.
        if let Some(deltas) = wasm_state_changes {
            let delta_hashes = deltas.calculate_hashes();
            warn!(
                self.log,
                "Executed update call: result  => [{}], deltas hash => [ wasm memory delta => {}, stable memory delta => {}, globals => {}]",
                wasm_execution_output,
                delta_hashes.0,
                delta_hashes.1,
                delta_hashes.2,
            );
        };
    }
}

/// A helper function that returns a Wasm execution result with an error.
pub fn wasm_execution_error(
    err: HypervisorError,
    num_instructions_left: NumInstructions,
) -> WasmExecutionResult {
    WasmExecutionResult::Finished(
        SliceExecutionOutput {
            executed_instructions: NumInstructions::from(0),
        },
        WasmExecutionOutput {
            wasm_result: Err(err),
            num_instructions_left,
            allocated_bytes: NumBytes::from(0),
            allocated_message_bytes: NumBytes::from(0),
            instance_stats: InstanceStats {
                accessed_pages: 0,
                dirty_pages: 0,
            },
        },
        None,
    )
}

/// Utility function to compute the page delta. It creates a copy of `Instance`
/// dirty pages. The function is public because it is used in
/// `wasmtime_random_memory_writes` tests.
#[doc(hidden)]
pub fn compute_page_delta<'a, S: SystemApi>(
    instance: &'a mut WasmtimeInstance<S>,
    dirty_pages: &[PageIndex],
) -> Vec<(PageIndex, &'a PageBytes)> {
    // heap pointer is only valid as long as the `Instance` is alive.
    let heap_addr: *const u8 = unsafe { instance.heap_addr() };

    let mut pages = vec![];

    for page_index in dirty_pages {
        let i = page_index.get();
        // SAFETY: All dirty pages are mapped and remain valid for the lifetime of
        // `instance`. Since this function is called after Wasm execution, the dirty
        // pages are not borrowed as mutable.
        let page_ref = unsafe {
            let offset: usize = i as usize * PAGE_SIZE;
            page_bytes_from_ptr(instance, (heap_addr as *const u8).add(offset))
        };
        pages.push((*page_index, page_ref));
    }
    pages
}

pub struct DirtyPageIndices {
    pub wasm_memory_delta: Vec<PageIndex>,
    pub stable_memory_delta: Vec<PageIndex>,
}

// A struct which holds the changes of the wasm state resulted from execution.
pub struct WasmStateChanges {
    pub dirty_page_indices: DirtyPageIndices,
    pub globals: Vec<Global>,
}

impl WasmStateChanges {
    fn new(
        wasm_memory_delta: Vec<PageIndex>,
        stable_memory_delta: Vec<PageIndex>,
        globals: Vec<Global>,
    ) -> Self {
        Self {
            dirty_page_indices: DirtyPageIndices {
                wasm_memory_delta,
                stable_memory_delta,
            },
            globals,
        }
    }

    // Only used when collecting information based on the result of message execution.
    //
    // See `collect_logs_after_execution`.
    fn calculate_hashes(&self) -> (u64, u64, u64) {
        fn hash<T: Hash>(x: &[T]) -> u64 {
            let mut hasher = DefaultHasher::new();
            x.hash(&mut hasher);
            hasher.finish()
        }

        (
            hash(&self.dirty_page_indices.stable_memory_delta),
            hash(&self.dirty_page_indices.wasm_memory_delta),
            hash(&self.globals),
        )
    }
}

/// The returns the number guard pages reserved at the end of 4GiB Wasm address
/// space. Message execution fails with an out-of-memory error if it attempts to
/// use the reserved pages.
/// Currently the pages are reserved only for canisters compiled with a Motoko
/// compiler version 0.6.20 or older.
pub fn get_wasm_reserved_pages(execution_state: &ExecutionState) -> NumWasmPages {
    let motoko_marker = WasmMethod::Update("__motoko_async_helper".to_string());
    let motoko_compiler = "motoko:compiler";
    let is_motoko_canister = execution_state.exports_method(&motoko_marker);
    // Motoko compiler at or before 0.6.20 does not emit "motoko:compiler" section.
    let is_recent_motoko_compiler = execution_state
        .metadata
        .custom_sections()
        .contains_key(motoko_compiler);
    if is_motoko_canister && !is_recent_motoko_compiler {
        // The threshold of 16 Wasm pages was chosen after consulting with
        // the Motoko team.
        return NumWasmPages::from(16);
    }
    NumWasmPages::from(0)
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::type_complexity)]
pub fn process(
    func_ref: FuncRef,
    api_type: ApiType,
    canister_current_memory_usage: NumBytes,
    execution_parameters: ExecutionParameters,
    subnet_available_memory: SubnetAvailableMemory,
    sandbox_safe_system_state: SandboxSafeSystemState,
    embedder_cache: &EmbedderCache,
    embedder: &WasmtimeEmbedder,
    wasm_memory: &mut Memory,
    stable_memory: &mut Memory,
    globals: &[Global],
    logger: ReplicaLogger,
    wasm_reserved_pages: NumWasmPages,
    out_of_instructions_handler: Arc<dyn OutOfInstructionsHandler>,
) -> (
    SliceExecutionOutput,
    WasmExecutionOutput,
    Option<WasmStateChanges>,
    Result<WasmtimeInstance<SystemApiImpl>, SystemApiImpl>,
) {
    let canister_id = sandbox_safe_system_state.canister_id();
    let modification_tracking = api_type.modification_tracking();
    let system_api = SystemApiImpl::new(
        api_type,
        sandbox_safe_system_state,
        canister_current_memory_usage,
        execution_parameters,
        subnet_available_memory,
        stable_memory.clone(),
        out_of_instructions_handler,
        logger,
    );

    let first_slice_instruction_limit = system_api.slice_instruction_limit();

    let mut instance = match embedder.new_instance(
        canister_id,
        embedder_cache,
        globals,
        wasm_memory,
        stable_memory,
        modification_tracking,
        system_api,
    ) {
        Ok(instance) => instance,
        Err((err, system_api)) => {
            // TODO(RUN-269): The `num_instructions_left` should be set to
            // the limit, not zero here.
            return (
                SliceExecutionOutput {
                    executed_instructions: first_slice_instruction_limit,
                },
                WasmExecutionOutput {
                    wasm_result: Err(err),
                    num_instructions_left: NumInstructions::from(0),
                    allocated_bytes: NumBytes::from(0),
                    allocated_message_bytes: NumBytes::from(0),
                    instance_stats: InstanceStats {
                        accessed_pages: 0,
                        dirty_pages: 0,
                    },
                },
                None,
                Err(system_api),
            );
        }
    };

    // Set the instruction limit for the first slice.
    instance.set_instruction_counter(first_slice_instruction_limit.get() as i64);

    // Execute Wasm code until it finishes or exceeds the message instruction
    // limit. With deterministic time slicing, this call may execute multiple
    // slices before it returns.
    let run_result = instance.run(func_ref);

    // Get the executed/remaining instructions for the message and the slice.
    let instruction_counter = instance.instruction_counter();
    let system_api = &instance.store_data().system_api;
    let slice_instruction_limit = system_api.slice_instruction_limit();
    // Capping at the limit to preserve the existing behaviour. It should be
    // possible to remove capping after ensuring that all callers can handle
    // instructions executed being larger than the limit.
    let slice_instructions_executed = system_api
        .slice_instructions_executed(instruction_counter)
        .min(slice_instruction_limit);
    let message_instruction_limit = system_api.message_instruction_limit();
    // Capping at the limit to avoid an underflow when computing the remaining
    // instructions below.
    let message_instructions_executed = system_api
        .message_instructions_executed(instruction_counter)
        .min(message_instruction_limit);
    let message_instructions_left = message_instruction_limit - message_instructions_executed;

    let instance_stats = instance.get_stats();

    // Has the side effect of deallocating memory if message failed and
    // returning cycles from a request that wasn't sent.
    let mut wasm_result = instance
        .store_data_mut()
        .system_api
        .take_execution_result(run_result.as_ref().err());

    let wasm_heap_size_after = instance.heap_size();
    let wasm_heap_limit =
        NumWasmPages::from(wasmtime_environ::WASM32_MAX_PAGES as usize) - wasm_reserved_pages;

    if wasm_heap_size_after > wasm_heap_limit {
        wasm_result = Err(HypervisorError::WasmReservedPages);
    }

    let mut allocated_bytes = NumBytes::from(0);
    let mut allocated_message_bytes = NumBytes::from(0);

    let wasm_state_changes = match run_result {
        Ok(run_result) => {
            match modification_tracking {
                ModificationTracking::Track => {
                    // Update the Wasm memory and serialize the delta.
                    let wasm_memory_delta = wasm_memory
                        .page_map
                        .update(&compute_page_delta(&mut instance, &run_result.dirty_pages));
                    wasm_memory.size = instance.heap_size();

                    // Update the stable memory and serialize the delta.
                    let stable_memory_delta = stable_memory.page_map.update(
                        &instance
                            .store_data_mut()
                            .system_api
                            .stable_memory_dirty_pages(),
                    );
                    stable_memory.size = run_result.stable_memory_size;
                    allocated_bytes = instance.store_data().system_api.get_allocated_bytes();
                    allocated_message_bytes = instance
                        .store_data()
                        .system_api
                        .get_allocated_message_bytes();

                    Some(WasmStateChanges::new(
                        wasm_memory_delta,
                        stable_memory_delta,
                        run_result.exported_globals,
                    ))
                }
                ModificationTracking::Ignore => None,
            }
        }
        Err(_) => None,
    };

    (
        SliceExecutionOutput {
            executed_instructions: slice_instructions_executed,
        },
        WasmExecutionOutput {
            wasm_result,
            num_instructions_left: message_instructions_left,
            allocated_bytes,
            allocated_message_bytes,
            instance_stats,
        },
        wasm_state_changes,
        Ok(instance),
    )
}

/// Takes a validated and instrumented wasm module and updates the wasm memory
/// `PageMap`.  Returns the exported methods and globals, as well as wasm memory
/// delta and final wasm memory size.
///
/// The only wasm code that will be run is const evaluation of the wasm globals.
#[allow(clippy::type_complexity)]
pub fn get_initial_globals_and_memory(
    data_segments: &Segments,
    embedder_cache: &EmbedderCache,
    embedder: &WasmtimeEmbedder,
    wasm_page_map: &mut PageMap,
    canister_id: CanisterId,
) -> HypervisorResult<(Vec<Global>, Vec<PageIndex>, NumWasmPages)> {
    let wasm_memory_pages = data_segments.as_pages();

    // Step 1. Apply the initial memory pages to the page map.
    let wasm_memory_delta = wasm_page_map.update(
        &wasm_memory_pages
            .iter()
            .map(|(index, bytes)| (*index, bytes as &PageBytes))
            .collect::<Vec<(PageIndex, &PageBytes)>>(),
    );

    // Step 2. Instantiate the Wasm module to get the globals and the memory size.
    //
    // We are using the wasm instance to initialize the execution state properly.
    // SystemApi is needed when creating a Wasmtime instance because the Linker
    // will try to assemble a list of all imports used by the wasm module.
    //
    // However, there is no need to initialize a `SystemApiImpl`
    // as we don't execute any wasm instructions at this point,
    // so we use an empty SystemApi instead.
    let system_api = SystemApiEmpty;
    // This runs the module's `start` function, but instrumentation clears the
    // start section and re-exports the start function as `canister_start`.
    let mut instance = match embedder.new_instance(
        canister_id,
        embedder_cache,
        &[],
        &Memory::new(wasm_page_map.clone(), NumWasmPages::from(0)),
        &Memory::new(PageMap::default(), NumWasmPages::from(0)),
        ModificationTracking::Ignore,
        system_api,
    ) {
        Ok(instance) => instance,
        Err((err, _system_api)) => {
            return Err(err);
        }
    };

    Ok((
        instance.get_exported_globals(),
        wasm_memory_delta,
        instance.heap_size(),
    ))
}
