//! Tools for working with virtual and machine assembly code
mod alloc;
pub(crate) mod asm_alloc; // TODO
mod asm_eval;
mod asm_op;
mod choice;
mod lru;

pub mod dynasm;

pub(crate) use alloc::RegisterAllocator;

pub use asm_eval::AsmFloatEval;
pub use asm_op::AsmOp;
pub use choice::Choice;
