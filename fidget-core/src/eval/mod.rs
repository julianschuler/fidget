//! Evaluation, both generically and with a small local interpreter

pub mod asm;
mod choice;

pub mod float_slice;
pub mod grad_slice;
pub mod interval;
pub mod point;

// Re-export a few things
pub use choice::Choice;

/// Represents a "family" of evaluators (JIT, interpreter, etc)
pub trait EvalFamily {
    /// Register limit for this evaluator family.
    const REG_LIMIT: u8;

    type IntervalFunc: interval::IntervalFuncT;
    type FloatSliceFunc: float_slice::FloatSliceFuncT;
    type PointFunc: point::PointFuncT;
    type GradSliceFunc: grad_slice::GradSliceFuncT;
}