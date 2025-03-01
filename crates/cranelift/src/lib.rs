//! Support for compiling with Cranelift.
//!
//! This crate provides an implementation of the `wasmtime_environ::Compiler`
//! and `wasmtime_environ::CompilerBuilder` traits.

// # How does Wasmtime prevent stack overflow?
//
// A few locations throughout the codebase link to this file to explain
// interrupts and stack overflow. To start off, let's take a look at stack
// overflow. Wasm code is well-defined to have stack overflow being recoverable
// and raising a trap, so we need to handle this somehow! There's also an added
// constraint where as an embedder you frequently are running host-provided
// code called from wasm. WebAssembly and native code currently share the same
// call stack, so you want to make sure that your host-provided code will have
// enough call-stack available to it.
//
// Given all that, the way that stack overflow is handled is by adding a
// prologue check to all JIT functions for how much native stack is remaining.
// The `VMContext` pointer is the first argument to all functions, and the first
// field of this structure is `*const VMInterrupts` and the first field of that
// is the stack limit. Note that the stack limit in this case means "if the
// stack pointer goes below this, trap". Each JIT function which consumes stack
// space or isn't a leaf function starts off by loading the stack limit,
// checking it against the stack pointer, and optionally traps.
//
// This manual check allows the embedder (us) to give wasm a relatively precise
// amount of stack allocation. Using this scheme we reserve a chunk of stack
// for wasm code relative from where wasm code was called. This ensures that
// native code called by wasm should have native stack space to run, and the
// numbers of stack spaces here should all be configurable for various
// embeddings.
//
// Note that we do not consider each thread's stack guard page here. It's
// considered that if you hit that you still abort the whole program. This
// shouldn't happen most of the time because wasm is always stack-bound and
// it's up to the embedder to bound its own native stack.
//
// So all-in-all, that's how we implement stack checks. Note that stack checks
// cannot be disabled because it's a feature of core wasm semantics. This means
// that all functions almost always have a stack check prologue, and it's up to
// us to optimize away that cost as much as we can.
//
// For more information about the tricky bits of managing the reserved stack
// size of wasm, see the implementation in `traphandlers.rs` in the
// `update_stack_limit` function.
//
// # How is Wasmtime interrupted?
//
// Ok so given all that background of stack checks, the next thing we want to
// build on top of this is the ability to *interrupt* executing wasm code. This
// is useful to ensure that wasm always executes within a particular time slice
// or otherwise doesn't consume all CPU resources on a system. There are two
// major ways that interrupts are required:
//
// * Loops - likely immediately apparent but it's easy to write an infinite
//   loop in wasm, so we need the ability to interrupt loops.
// * Function entries - somewhat more subtle, but imagine a module where each
//   function calls the next function twice. This creates 2^n calls pretty
//   quickly, so a pretty small module can export a function with no loops
//   that takes an extremely long time to call.
//
// In many cases if an interrupt comes in you want to interrupt host code as
// well, but we're explicitly not considering that here. We're hoping that
// interrupting host code is largely left to the embedder (e.g. figuring out
// how to interrupt blocking syscalls) and they can figure that out. The purpose
// of this feature is to basically only give the ability to interrupt
// currently-executing wasm code (or triggering an interrupt as soon as wasm
// reenters itself).
//
// To implement interruption of loops we insert code at the head of all loops
// which checks the stack limit counter. If the counter matches a magical
// sentinel value that's impossible to be the real stack limit, then we
// interrupt the loop and trap. To implement interrupts of functions, we
// actually do the same thing where the magical sentinel value we use here is
// automatically considered as considering all stack pointer values as "you ran
// over your stack". This means that with a write of a magical value to one
// location we can interrupt both loops and function bodies.
//
// The "magical value" here is `usize::max_value() - N`. We reserve
// `usize::max_value()` for "the stack limit isn't set yet" and so -N is
// then used for "you got interrupted". We do a bit of patching afterwards to
// translate a stack overflow into an interrupt trap if we see that an
// interrupt happened. Note that `N` here is a medium-size-ish nonzero value
// chosen in coordination with the cranelift backend. Currently it's 32k. The
// value of N is basically a threshold in the backend for "anything less than
// this requires only one branch in the prologue, any stack size bigger requires
// two branches". Naturally we want most functions to have one branch, but we
// also need to actually catch stack overflow, so for now 32k is chosen and it's
// assume no valid stack pointer will ever be `usize::max_value() - 32k`.

use cranelift_codegen::ir;
use cranelift_codegen::isa::{CallConv, TargetIsa};
use cranelift_wasm::{FuncIndex, WasmFuncType, WasmType};
use target_lexicon::CallingConvention;
use wasmtime_environ::{Module, TypeTables};

pub use builder::builder;

mod builder;
mod compiler;
mod func_environ;
mod obj;

/// Creates a new cranelift `Signature` with no wasm params/results for the
/// given calling convention.
///
/// This will add the default vmctx/etc parameters to the signature returned.
fn blank_sig(isa: &dyn TargetIsa, call_conv: CallConv) -> ir::Signature {
    let pointer_type = isa.pointer_type();
    let mut sig = ir::Signature::new(call_conv);
    // Add the caller/callee `vmctx` parameters.
    sig.params.push(ir::AbiParam::special(
        pointer_type,
        ir::ArgumentPurpose::VMContext,
    ));
    sig.params.push(ir::AbiParam::new(pointer_type));
    return sig;
}

/// Returns the default calling convention for the `isa` provided.
///
/// Note that this calling convention is used for exported functions.
fn wasmtime_call_conv(isa: &dyn TargetIsa) -> CallConv {
    match isa.triple().default_calling_convention() {
        Ok(CallingConvention::AppleAarch64) => CallConv::WasmtimeAppleAarch64,
        Ok(CallingConvention::SystemV) | Err(()) => CallConv::WasmtimeSystemV,
        Ok(CallingConvention::WindowsFastcall) => CallConv::WasmtimeFastcall,
        Ok(unimp) => unimplemented!("calling convention: {:?}", unimp),
    }
}

/// Appends the types of the `wasm` function signature into the `sig` signature
/// provided.
///
/// Typically the `sig` signature will have been created from [`blank_sig`]
/// above.
fn push_types(isa: &dyn TargetIsa, sig: &mut ir::Signature, wasm: &WasmFuncType) {
    let cvt = |ty: &WasmType| ir::AbiParam::new(value_type(isa, *ty));
    sig.params.extend(wasm.params.iter().map(&cvt));
    sig.returns.extend(wasm.returns.iter().map(&cvt));
}

/// Returns the corresponding cranelift type for the provided wasm type.
fn value_type(isa: &dyn TargetIsa, ty: WasmType) -> ir::types::Type {
    match ty {
        WasmType::I32 => ir::types::I32,
        WasmType::I64 => ir::types::I64,
        WasmType::F32 => ir::types::F32,
        WasmType::F64 => ir::types::F64,
        WasmType::V128 => ir::types::I8X16,
        WasmType::FuncRef | WasmType::ExternRef => {
            wasmtime_environ::reference_type(ty, isa.pointer_type())
        }
        WasmType::ExnRef => unimplemented!(),
    }
}

/// Returns a cranelift signature suitable to indirectly call the wasm signature
/// specified by `wasm`.
///
/// This will implicitly use the default calling convention for `isa` since to
/// indirectly call a wasm function it must be possibly exported somehow (e.g.
/// this assumes the function target to call doesn't use the "fast" calling
/// convention).
fn indirect_signature(isa: &dyn TargetIsa, wasm: &WasmFuncType) -> ir::Signature {
    let mut sig = blank_sig(isa, wasmtime_call_conv(isa));
    push_types(isa, &mut sig, wasm);
    return sig;
}

/// Returns the cranelift fucntion signature of the function specified.
///
/// Note that this will determine the calling convention for the function, and
/// namely includes an optimization where functions never exported from a module
/// use a custom theoretically faster calling convention instead of the default.
fn func_signature(
    isa: &dyn TargetIsa,
    module: &Module,
    types: &TypeTables,
    index: FuncIndex,
) -> ir::Signature {
    let call_conv = match module.defined_func_index(index) {
        // If this is a defined function in the module and it's never possibly
        // exported, then we can optimize this function to use the fastest
        // calling convention since it's purely an internal implementation
        // detail of the module itself.
        Some(idx) if !module.possibly_exported_funcs.contains(&idx) => CallConv::Fast,

        // ... otherwise if it's an imported function or if it's a possibly
        // exported function then we use the default ABI wasmtime would
        // otherwise select.
        _ => wasmtime_call_conv(isa),
    };
    let mut sig = blank_sig(isa, call_conv);
    push_types(
        isa,
        &mut sig,
        &types.wasm_signatures[module.functions[index]],
    );
    return sig;
}
