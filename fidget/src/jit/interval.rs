use crate::{
    eval::types::Interval,
    jit::{
        mmap::Mmap, reg, AssemblerData, AssemblerT, JitTracingEval,
        CHOICE_BOTH, CHOICE_LEFT, CHOICE_RIGHT, IMM_REG, OFFSET,
        REGISTER_LIMIT,
    },
    Error,
};
use dynasmrt::{dynasm, DynasmApi, DynasmLabelApi};

/// Assembler for interval evaluation
///
/// The resulting function has the following signature:
/// ```
/// # type IntervalFn =
/// extern "C" fn(
///    [f32; 2], // X (s0, s1)
///    [f32; 2], // Y (s2, s3)
///    [f32; 2], // Z (s4, s5)
///    *const f32, // vars (X0)
///    *mut u8, // choices (X1)
///    *mut u8, // simplify (X2)
///) -> [f32; 2];
/// ```
///
/// The first three arguments are X, Y, and Z intervals.  They come packed into
/// `s0-5`, and we shuffle them into SIMD registers `V0.2S`, `V1.2S`, and
/// `V2.2s` respectively.
///
/// During evaluation, each SIMD register stores an interval.  `s[0]` is the
/// lower bound of the interval and `s[1]` is the upper bound; for example,
/// `V0.S0` represents the lower bound for X.
pub struct IntervalAssembler(AssemblerData<[f32; 2]>);

#[cfg(target_arch = "aarch64")]
impl AssemblerT for IntervalAssembler {
    type Data = Interval;

    fn init(mmap: Mmap, slot_count: usize) -> Self {
        let mut out = AssemblerData::new(mmap);
        dynasm!(out.ops
            // Preserve frame and link register
            ; stp   x29, x30, [sp, #-16]!
            // Preserve sp
            ; mov   x29, sp
            // Preserve callee-saved floating-point registers
            ; stp   d8, d9, [sp, #-16]!
            ; stp   d10, d11, [sp, #-16]!
            ; stp   d12, d13, [sp, #-16]!
            ; stp   d14, d15, [sp, #-16]!

            // Arguments are passed in S0-5; collect them into V0-1
            ; mov v0.s[1], v1.s[0]
            ; mov v1.s[0], v2.s[0]
            ; mov v1.s[1], v3.s[0]
            ; mov v2.s[0], v4.s[0]
            ; mov v2.s[1], v5.s[0]
        );
        out.prepare_stack(slot_count);
        Self(out)
    }
    /// Reads from `src_mem` to `dst_reg`
    fn build_load(&mut self, dst_reg: u8, src_mem: u32) {
        assert!(dst_reg < REGISTER_LIMIT);
        let sp_offset = self.0.stack_pos(src_mem);
        assert!(sp_offset <= 32768);
        dynasm!(self.0.ops ; ldr D(reg(dst_reg)), [sp, #(sp_offset)])
    }
    /// Writes from `src_reg` to `dst_mem`
    fn build_store(&mut self, dst_mem: u32, src_reg: u8) {
        assert!(src_reg < REGISTER_LIMIT);
        let sp_offset = self.0.stack_pos(dst_mem);
        assert!(sp_offset <= 32768);
        dynasm!(self.0.ops ; str D(reg(src_reg)), [sp, #(sp_offset)])
    }
    /// Copies the given input to `out_reg`
    fn build_input(&mut self, out_reg: u8, src_arg: u8) {
        dynasm!(self.0.ops ; fmov D(reg(out_reg)), D(src_arg as u32));
    }
    fn build_var(&mut self, out_reg: u8, src_arg: u32) {
        assert!(src_arg * 4 < 16384);
        dynasm!(self.0.ops
            ; ldr w15, [x0, #(src_arg * 4)]
            ; dup V(reg(out_reg)).s2, w15
        );
    }
    fn build_copy(&mut self, out_reg: u8, lhs_reg: u8) {
        dynasm!(self.0.ops ; fmov D(reg(out_reg)), D(reg(lhs_reg)))
    }
    fn build_neg(&mut self, out_reg: u8, lhs_reg: u8) {
        dynasm!(self.0.ops
            ; fneg V(reg(out_reg)).s2, V(reg(lhs_reg)).s2
            ; rev64 V(reg(out_reg)).s2, V(reg(out_reg)).s2
        )
    }
    fn build_abs(&mut self, out_reg: u8, lhs_reg: u8) {
        dynasm!(self.0.ops
            // Store lhs < 0.0 in x15
            ; fcmle v4.s2, V(reg(lhs_reg)).s2, #0.0
            ; fmov x15, d4

            // Store abs(lhs) in V(reg(out_reg))
            ; fabs V(reg(out_reg)).s2, V(reg(lhs_reg)).s2

            // Check whether lhs.upper < 0
            ; tst x15, #0x1_0000_0000
            ; b.ne #24 // -> upper_lz

            // Check whether lhs.lower < 0
            ; tst x15, #0x1

            // otherwise, we're good; return the original
            ; b.eq #20 // -> end

            // if lhs.lower < 0, then the output is
            //  [0.0, max(abs(lower, upper))]
            ; movi d4, #0
            ; fmaxnmv s4, V(reg(out_reg)).s4
            ; fmov D(reg(out_reg)), d4
            // Fall through to do the swap

            // <- upper_lz
            // if upper < 0
            //   return [-upper, -lower]
            ; rev64 V(reg(out_reg)).s2, V(reg(out_reg)).s2

            // <- end
        )
    }
    fn build_recip(&mut self, out_reg: u8, lhs_reg: u8) {
        let nan_u32 = f32::NAN.to_bits();
        dynasm!(self.0.ops
            // Check whether lhs.lower > 0.0
            ; fcmp S(reg(lhs_reg)), 0.0
            ; b.gt #32 // -> okay

            // Check whether lhs.upper < 0.0
            ; mov s4, V(reg(lhs_reg)).s[1]
            ; fcmp s4, 0.0
            ; b.mi #20 // -> okay

            // Bad case: the division spans 0, so return NaN
            ; movz w15, #(nan_u32 >> 16), lsl 16
            ; movk w15, #(nan_u32)
            ; dup V(reg(out_reg)).s2, w15
            ; b #20 // -> end

            // <- okay
            ; fmov s4, #1.0
            ; dup v4.s2, v4.s[0]
            ; fdiv V(reg(out_reg)).s2, v4.s2, V(reg(lhs_reg)).s2
            ; rev64 V(reg(out_reg)).s2, V(reg(out_reg)).s2

            // <- end
        )
    }
    fn build_sqrt(&mut self, out_reg: u8, lhs_reg: u8) {
        let nan_u32 = f32::NAN.to_bits();
        dynasm!(self.0.ops
            // Store lhs <= 0.0 in x15
            ; fcmle v4.s2, V(reg(lhs_reg)).s2, #0.0
            ; fmov x15, d4

            // Check whether lhs.upper < 0
            ; tst x15, #0x1_0000_0000
            ; b.ne #40 // -> upper_lz

            ; tst x15, #0x1
            ; b.ne #12 // -> lower_lz

            // Happy path
            ; fsqrt V(reg(out_reg)).s2, V(reg(lhs_reg)).s2
            ; b #36 // -> end

            // <- lower_lz
            ; mov v4.s[0], V(reg(lhs_reg)).s[1]
            ; fsqrt s4, s4
            ; movi D(reg(out_reg)), #0
            ; mov V(reg(out_reg)).s[1], v4.s[0]
            ; b #16

            // <- upper_lz
            ; movz w9, #(nan_u32 >> 16), lsl 16
            ; movk w9, #(nan_u32)
            ; dup V(reg(out_reg)).s2, w9

            // <- end
        )
    }
    fn build_square(&mut self, out_reg: u8, lhs_reg: u8) {
        dynasm!(self.0.ops
            // Store lhs <= 0.0 in x15
            ; fcmle v4.s2, V(reg(lhs_reg)).s2, #0.0
            ; fmov x15, d4
            ; fmul V(reg(out_reg)).s2, V(reg(lhs_reg)).s2, V(reg(lhs_reg)).s2

            // Check whether lhs.upper <= 0.0
            ; tst x15, #0x1_0000_0000
            ; b.ne #28 // -> swap

            // Test whether lhs.lower <= 0.0
            ; tst x15, #0x1
            ; b.eq #24 // -> end

            // If the input interval straddles 0, then the
            // output is [0, max(lower**2, upper**2)]
            ; fmaxnmv s4, V(reg(out_reg)).s4
            ; movi D(reg(out_reg)), #0
            ; mov V(reg(out_reg)).s[1], v4.s[0]
            ; b #8 // -> end

            // <- swap
            ; rev64 V(reg(out_reg)).s2, V(reg(out_reg)).s2

            // <- end
        )
    }
    fn build_add(&mut self, out_reg: u8, lhs_reg: u8, rhs_reg: u8) {
        dynasm!(self.0.ops
            ; fadd V(reg(out_reg)).s2, V(reg(lhs_reg)).s2, V(reg(rhs_reg)).s2
        )
    }
    fn build_sub(&mut self, out_reg: u8, lhs_reg: u8, rhs_reg: u8) {
        dynasm!(self.0.ops
            ; rev64 v4.s2, V(reg(rhs_reg)).s2
            ; fsub V(reg(out_reg)).s2, V(reg(lhs_reg)).s2, v4.s2
        )
    }
    fn build_sub_reg_imm(&mut self, out_reg: u8, arg: u8, imm: f32) {
        let imm = self.load_imm(imm);
        dynasm!(self.0.ops
            ; fsub V(reg(out_reg)).s2, V(reg(arg)).s2, V(reg(imm)).s2
        )
    }
    fn build_mul(&mut self, out_reg: u8, lhs_reg: u8, rhs_reg: u8) {
        dynasm!(self.0.ops
            // Set up v4 to contain
            //  [lhs.upper, lhs.lower, lhs.lower, lhs.upper]
            // and v5 to contain
            //  [rhs.upper, rhs.lower, rhs.upper, rhs.upper]
            //
            // Multiplying them out will hit all four possible
            // combinations; then we extract the min and max
            // with vector-reducing operations
            ; rev64 v4.s2, V(reg(lhs_reg)).s2
            ; mov v4.d[1], V(reg(lhs_reg)).d[0]
            ; dup v5.d2, V(reg(rhs_reg)).d[0]

            ; fmul v4.s4, v4.s4, v5.s4
            ; fminnmv S(reg(out_reg)), v4.s4
            ; fmaxnmv s5, v4.s4
            ; mov V(reg(out_reg)).s[1], v5.s[0]
        )
    }

    fn build_mul_imm(&mut self, out_reg: u8, lhs_reg: u8, imm: f32) {
        let rhs_reg = self.load_imm(imm);
        dynasm!(self.0.ops
            ; fmul V(reg(out_reg)).s2, V(reg(lhs_reg)).s2, V(reg(rhs_reg)).s2
        );
        if imm < 0.0 {
            dynasm!(self.0.ops
                ; rev64 V(reg(out_reg)).s2, V(reg(out_reg)).s2
            );
        }
    }
    fn build_div(&mut self, out_reg: u8, lhs_reg: u8, rhs_reg: u8) {
        let nan_u32 = f32::NAN.to_bits();
        dynasm!(self.0.ops
            // Store rhs.lower > 0.0 in x15, then check rhs.lower > 0
            ; fcmp S(reg(rhs_reg)), #0.0
            ; b.gt #32 // -> happy

            // Store rhs.upper < 0.0 in x15, then check rhs.upper < 0
            ; mov s4, V(reg(rhs_reg)).s[1]
            ; fcmp s4, #0.0
            ; b.lt #20

            // Sad path: rhs spans 0, so the output includes NaN
            ; movz w9, #(nan_u32 >> 16), lsl 16
            ; movk w9, #(nan_u32)
            ; dup V(reg(out_reg)).s2, w9
            ; b #32 // -> end

            // >happy:
            // Set up v4 to contain
            //  [lhs.upper, lhs.lower, lhs.lower, lhs.upper]
            // and v5 to contain
            //  [rhs.upper, rhs.lower, rhs.upper, rhs.upper]
            //
            // Dividing them out will hit all four possible
            // combinations; then we extract the min and max
            // with vector-reducing operations
            ; rev64 v4.s2, V(reg(lhs_reg)).s2
            ; mov v4.d[1], V(reg(lhs_reg)).d[0]
            ; dup v5.d2, V(reg(rhs_reg)).d[0]

            ; fdiv v4.s4, v4.s4, v5.s4
            ; fminnmv S(reg(out_reg)), v4.s4
            ; fmaxnmv s5, v4.s4
            ; mov V(reg(out_reg)).s[1], v5.s[0]

            // >end
        )
    }
    fn build_max(&mut self, out_reg: u8, lhs_reg: u8, rhs_reg: u8) {
        dynasm!(self.0.ops
            // Basically the same as MinRegReg
            ; zip2 v4.s2, V(reg(lhs_reg)).s2, V(reg(rhs_reg)).s2
            ; zip1 v5.s2, V(reg(rhs_reg)).s2, V(reg(lhs_reg)).s2
            ; fcmgt v5.s2, v5.s2, v4.s2
            ; fmov x15, d5
            ; ldrb w14, [x1]

            ; tst x15, #0x1_0000_0000
            ; b.ne #28 // -> lhs

            ; tst x15, #0x1
            ; b.eq #36 // -> both

            // LHS < RHS
            ; fmov D(reg(out_reg)), D(reg(rhs_reg))
            ; orr w14, w14, #CHOICE_RIGHT
            ; strb w14, [x2, #0] // write a non-zero value to simplify
            ; b #28 // -> end

            // <- lhs (when RHS < LHS)
            ; fmov D(reg(out_reg)), D(reg(lhs_reg))
            ; orr w14, w14, #CHOICE_LEFT
            ; strb w14, [x2, #0] // write a non-zero value to simplify
            ; b #12 // -> end

            // <- both
            ; fmax V(reg(out_reg)).s2, V(reg(lhs_reg)).s2, V(reg(rhs_reg)).s2
            ; orr w14, w14, #CHOICE_BOTH

            // <- end
            ; strb w14, [x1], #1 // post-increment
        )
    }
    fn build_min(&mut self, out_reg: u8, lhs_reg: u8, rhs_reg: u8) {
        dynasm!(self.0.ops
            //  if lhs.upper < rhs.lower
            //      *choices++ |= CHOICE_LEFT
            //      out = lhs
            //  elif rhs.upper < lhs.lower
            //      *choices++ |= CHOICE_RIGHT
            //      out = rhs
            //  else
            //      *choices++ |= CHOICE_BOTH
            //      out = fmin(lhs, rhs)

            // v4 = [lhs.upper, rhs.upper]
            // v5 = [rhs.lower, lhs.lower]
            // This lets us do two comparisons simultaneously
            ; zip2 v4.s2, V(reg(lhs_reg)).s2, V(reg(rhs_reg)).s2
            ; zip1 v5.s2, V(reg(rhs_reg)).s2, V(reg(lhs_reg)).s2

            // v5 = [rhs.lower > lhs.upper, lhs.lower > rhs.upper]
            ; fcmgt v5.s2, v5.s2, v4.s2
            ; fmov x15, d5
            ; ldrb w14, [x1]

            ; tst x15, #0x1_0000_0000
            ; b.ne #28 // -> rhs

            ; tst x15, #0x1
            ; b.eq #36 // -> both

            // Fallthrough: LHS < RHS
            ; fmov D(reg(out_reg)), D(reg(lhs_reg))
            ; orr w14, w14, #CHOICE_LEFT
            ; strb w14, [x2, #0] // write a non-zero value to simplify
            ; b #28 // -> end

            // <- rhs (for when RHS < LHS)
            ; fmov D(reg(out_reg)), D(reg(rhs_reg))
            ; orr w14, w14, #CHOICE_RIGHT
            ; strb w14, [x2, #0] // write a non-zero value to simplify
            ; b #12

            // <- both
            ; fmin V(reg(out_reg)).s2, V(reg(lhs_reg)).s2, V(reg(rhs_reg)).s2
            ; orr w14, w14, #CHOICE_BOTH

            // <- end
            ; strb w14, [x1], #1 // post-increment
        )
    }

    /// Loads an immediate into register S4, using W9 as an intermediary
    fn load_imm(&mut self, imm: f32) -> u8 {
        let imm_u32 = imm.to_bits();
        dynasm!(self.0.ops
            ; movz w15, #(imm_u32 >> 16), lsl 16
            ; movk w15, #(imm_u32)
            ; dup V(IMM_REG as u32).s2, w15
        );
        IMM_REG.wrapping_sub(OFFSET)
    }

    fn finalize(mut self, out_reg: u8) -> Result<Mmap, Error> {
        assert!(self.0.mem_offset < 4096);
        dynasm!(self.0.ops
            // Prepare our return value
            ; mov  s0, V(reg(out_reg)).s[0]
            ; mov  s1, V(reg(out_reg)).s[1]
            // Restore stack space used for spills
            ; add   sp, sp, #(self.0.mem_offset as u32)
            // Restore callee-saved floating-point registers
            ; ldp   d14, d15, [sp], #16
            ; ldp   d12, d13, [sp], #16
            ; ldp   d10, d11, [sp], #16
            ; ldp   d8, d9, [sp], #16
            // Restore frame and link register
            ; ldp   x29, x30, [sp], #16
            ; ret
        );

        self.0.ops.finalize()
    }
}

/// Registers are passed in as follows
/// | Variable   | Register | Type               |
/// |------------|----------|--------------------|
/// | X          | `xmm0`   | `[f32; 2]`         |
/// | Y          | `xmm1`   | `[f32; 2]`         |
/// | Z          | `xmm2`   | `[f32; 2]`         |
/// | `vars`     | `rdi`    | `*const f32`       |
/// | `choices`  | `rsi`    | `*mut u8` (array)  |
/// | `simplify` | `rdx`    | `*mut u8` (single) |
#[cfg(target_arch = "x86_64")]
impl AssemblerT for IntervalAssembler {
    type Data = Interval;

    fn init(mmap: Mmap, slot_count: usize) -> Self {
        let mut out = AssemblerData::new(mmap);
        dynasm!(out.ops
            ; push rbp
            ; mov rbp, rsp

            // Put X/Y/Z on the stack so we can use those registers
            ; movq [rbp - 8], xmm0
            ; movq [rbp - 16], xmm1
            ; movq [rbp - 24], xmm2
        );
        out.prepare_stack(slot_count);
        Self(out)
    }
    fn build_load(&mut self, dst_reg: u8, src_mem: u32) {
        assert!(dst_reg < REGISTER_LIMIT);
        let sp_offset: i32 = self.0.stack_pos(src_mem).try_into().unwrap();
        dynasm!(self.0.ops
            // Pretend that we're a double
            ; movq Rx(reg(dst_reg)), [rsp + sp_offset]
        );
    }
    fn build_store(&mut self, dst_mem: u32, src_reg: u8) {
        assert!(src_reg < REGISTER_LIMIT);
        let sp_offset: i32 = self.0.stack_pos(dst_mem).try_into().unwrap();
        dynasm!(self.0.ops
            // Pretend that we're a double
            ; movq [rsp + sp_offset], Rx(reg(src_reg))
        );
    }
    fn build_input(&mut self, out_reg: u8, src_arg: u8) {
        dynasm!(self.0.ops
            ; movq Rx(reg(out_reg)), [rbp - 8 * (src_arg as i32 + 1)]
        );
    }
    fn build_var(&mut self, out_reg: u8, src_arg: u32) {
        dynasm!(self.0.ops
            ; movss Rx(reg(out_reg)), [rdi + 4 * (src_arg as i32)]
            // Somewhat overkill, since we only need two values, but oh well
            ; vbroadcastss Rx(reg(out_reg)), Rx(reg(out_reg))
        );
    }
    fn build_copy(&mut self, out_reg: u8, lhs_reg: u8) {
        dynasm!(self.0.ops
            ; movq Rx(reg(out_reg)), Rx(reg(lhs_reg))
        );
    }
    fn build_neg(&mut self, out_reg: u8, lhs_reg: u8) {
        dynasm!(self.0.ops
            ; pshufd Rx(reg(out_reg)), Rx(reg(lhs_reg)), 0b11110001u8 as i8
            ; pcmpeqd xmm0, xmm0 // set xmm0 to all 1s
            ; pslld xmm0, 31     // shift, leaving xmm0 = 0x80000000
            ; vbroadcastss xmm0, xmm0 // Smear this onto every f32
            ; xorps Rx(reg(out_reg)), xmm0
        );
    }
    fn build_abs(&mut self, out_reg: u8, lhs_reg: u8) {
        // TODO: use cmpltss instead of 2x comiss?
        dynasm!(self.0.ops
            // Store 0.0 to xmm0, for comparisons
            ; pxor xmm0, xmm0

            // Pull the upper value into xmm1
            ; movq rax, Rx(reg(lhs_reg))
            ; shr rax, 32
            ; movd xmm1, eax

            // Check whether lhs.upper < 0
            ; comiss xmm0, xmm1
            ; ja >neg

            // Check whether lhs.lower < 0
            ; comiss xmm0, Rx(reg(lhs_reg))
            ; ja >straddle

            // Fallthrough: the whole interval is above zero, so we just copy it
            // over and return.
            ; movq Rx(reg(out_reg)), Rx(reg(lhs_reg))
            ; jmp >end

            // The interval is less than zero, so we need to calculate
            // [-upper, -lower]
            ; neg:
            ; pcmpeqd xmm0, xmm0 // set xmm0 to all 1s
            ; pslld xmm0, 31     // shift, leaving xmm0 = 0x80000000
            ; vbroadcastss xmm0, xmm0 // Smear this onto every f32
            ; xorps xmm0, Rx(reg(lhs_reg)) // xor to swap sign bits
            ; pshufd xmm0, xmm0, 1 // swap lo and hi
            ; movq Rx(reg(out_reg)), xmm0
            ; jmp >end

            // The interval straddles 0, so we need to calculate
            // [0.0, max(abs(lower, upper))]
            ; straddle:
            ; pcmpeqd xmm0, xmm0 // set xmm0 to all 1s
            ; psrld xmm0, 1      // shift, leaving xmm0 = 0x7fffffff
            ; vbroadcastss xmm0, xmm0 // Smear this onto every f32

            // Copy to out_reg and clear sign bits; setting up out_reg as
            // [abs(low), abs(high)]
            ; movq Rx(reg(out_reg)), Rx(reg(lhs_reg))
            ; andps Rx(reg(out_reg)), xmm0

            // Set up xmm0 to contain [abs(high), abs(low)]
            ; pshufd xmm0, Rx(reg(out_reg)), 0b11110001u8 as i8

            ; comiss xmm0, Rx(reg(out_reg)) // Compare abs(hi) vs abs(lo)
            ; ja >clr // if abs(hi) > abs(lo), then we don't need to swap

            ; pshufd Rx(reg(out_reg)), Rx(reg(out_reg)), 0b11110011u8 as i8

            // Clear the lowest value of the interval, leaving us with [0, ...]
            ; clr:
            ; pshufd Rx(reg(out_reg)), Rx(reg(out_reg)), 0b11110111u8 as i8
            // fallthrough to end

            ; end:
        );
    }
    fn build_recip(&mut self, out_reg: u8, lhs_reg: u8) {
        let nan_u32 = f32::NAN.to_bits();
        let one_u32 = 1f32.to_bits();
        dynasm!(self.0.ops
            ; pxor xmm0, xmm0 // xmm0 = 0.0
            ; comiss Rx(reg(lhs_reg)), xmm0
            ; ja >okay // low element is > 0
            ; pshufd xmm1, Rx(reg(lhs_reg)), 1 // extract high element
            ; comiss xmm0, xmm1
            ; ja >okay // high element is < 0

            // Bad case: the division spans 0, so return NaN
            ; mov eax, nan_u32 as i32
            ; movd Rx(reg(out_reg)), eax
            ; vbroadcastss Rx(reg(out_reg)), Rx(reg(out_reg))
            ; jmp >end

            ; okay:
            ; mov eax, one_u32 as i32
            ; movd xmm0, eax
            ; vbroadcastss xmm0, xmm0
            ; vdivps Rx(reg(out_reg)), xmm0, Rx(reg(lhs_reg))
            ; pshufd Rx(reg(out_reg)), Rx(reg(out_reg)), 0b0001
            // Fallthrough to end

            ; end:
        );
    }
    fn build_sqrt(&mut self, out_reg: u8, lhs_reg: u8) {
        let nan_u32 = f32::NAN.to_bits();
        dynasm!(self.0.ops
            ; pxor xmm0, xmm0 // xmm0 = 0.0
            ; pshufd xmm1, Rx(reg(lhs_reg)), 1
            ; comiss xmm0, xmm1
            ; ja >upper_lz
            ; comiss xmm0, Rx(reg(lhs_reg))
            ; ja >lower_lz

            // Happy path
            ; vsqrtps Rx(reg(out_reg)), Rx(reg(lhs_reg))
            ; jmp >end

            // lower < 0, upper > 0 => [0, sqrt(upper)]
            ; lower_lz:
            ; pxor xmm0, xmm0 // clear xmm0
            ; sqrtss xmm0, xmm1
            ; pshufd Rx(reg(out_reg)), xmm0, 0b11110011u8 as i8
            ; jmp >end

            // upper < 0 => [NaN, NaN]
            ; upper_lz:
            ; mov eax, nan_u32 as i32
            ; movd Rx(reg(out_reg)), eax
            ; vbroadcastss Rx(reg(out_reg)), Rx(reg(out_reg))

            ; end:
        );
    }
    fn build_square(&mut self, out_reg: u8, lhs_reg: u8) {
        dynasm!(self.0.ops
            // Put component-wise multiplication in xmm2
            ; vmulps xmm2, Rx(reg(lhs_reg)), Rx(reg(lhs_reg))
            ; pxor xmm0, xmm0 // xmm0 = 0.0
            ; pshufd xmm1, Rx(reg(lhs_reg)), 1
            ; comiss xmm0, xmm1
            ; ja >neg
            ; comiss xmm0, Rx(reg(lhs_reg))
            ; ja >straddle

            // Fallthrough: lower > 0, so our previous result is fine
            ; movq Rx(reg(out_reg)), xmm2
            ; jmp >end

            // upper < 0, so we square then swap
            ; neg:
            ; pshufd Rx(reg(out_reg)), xmm2, 0b11110001u8 as i8
            ; jmp >end

            // lower < 0, upper > 0 => pick the bigger result
            ; straddle:
            ; pshufd xmm0, xmm2, 1
            ; maxss xmm0, xmm2
            ; movq rax, xmm0
            ; shl rax, 32 // Shift to put zeros in lower, square in upper
            ; movq Rx(reg(out_reg)), rax

            ; end:
        );
    }
    fn build_add(&mut self, out_reg: u8, lhs_reg: u8, rhs_reg: u8) {
        dynasm!(self.0.ops
            ; vaddps Rx(reg(out_reg)), Rx(reg(lhs_reg)), Rx(reg(rhs_reg))
        );
    }
    fn build_sub(&mut self, out_reg: u8, lhs_reg: u8, rhs_reg: u8) {
        dynasm!(self.0.ops
            ; pshufd xmm1, Rx(reg(rhs_reg)), 0b11110001u8 as i8
            ; vsubps Rx(reg(out_reg)), Rx(reg(lhs_reg)), xmm1
        );
    }
    fn build_mul(&mut self, out_reg: u8, lhs_reg: u8, rhs_reg: u8) {
        dynasm!(self.0.ops
            ; pshufd xmm2, Rx(reg(lhs_reg)), 0b01000001_i8
            ; pshufd xmm1, Rx(reg(rhs_reg)), 0b00010001_i8
            ; vmulps xmm2, xmm2, xmm1 // xmm2 contains all 4 results

            // Extract the horizontal maximum into out
            ; pshufd xmm1, xmm2, 0b00001110 // xmm1 = [_, _, 3, 2]
            ; vminps xmm1, xmm1, xmm2 // xmm1 = [_, _, min(3, 1), min(2, 0)]
            ; pshufd Rx(reg(out_reg)), xmm1, 0b00000001 // out = max(3, 1)
            ; minss Rx(reg(out_reg)), xmm1 // out[0] is lowest value

            // Extract the horizontal minimum into xmm2
            ; pshufd xmm1, xmm2, 0b00001110 // xmm1 = [_, _, 3, 2]
            ; vmaxps xmm1, xmm1, xmm2 // xmm1 = [_, _, max(3, 1), max(2, 0)]
            ; pshufd xmm2, xmm1, 0b00000001 // xmm2 = max(3, 1)
            ; maxss xmm2, xmm1 // xmm2[0] is highest value

            // Splice the two together
            // TODO is there a better way to do this?
            ; movd eax, xmm2
            ; shl rax, 32
            ; movd ecx, Rx(reg(out_reg))
            ; or rax, rcx
            ; movq Rx(reg(out_reg)), rax
        );
    }
    fn build_div(&mut self, out_reg: u8, lhs_reg: u8, rhs_reg: u8) {
        dynasm!(self.0.ops
            ; pxor xmm1, xmm1 // xmm1 = 0.0
            ; comiss Rx(reg(rhs_reg)), xmm1
            ; ja >okay
            ; pshufd xmm2, Rx(reg(rhs_reg)), 1
            ; comiss xmm1, xmm2
            ; ja >okay

            // Fallthrough: an input is NaN or rhs_reg spans 0; return NaN
            ; mov eax, std::f32::NAN.to_bits() as i32
            ; movd Rx(reg(out_reg)), eax
            ; vbroadcastss Rx(reg(out_reg)), Rx(reg(out_reg))
            ; jmp >end

            ; okay:
            ; pshufd xmm2, Rx(reg(lhs_reg)), 0b01000001_i8
            ; pshufd xmm1, Rx(reg(rhs_reg)), 0b00010001_i8
            ; vdivps xmm2, xmm2, xmm1 // xmm2 contains all 4 results

            // Extract the horizontal maximum into out
            ; pshufd xmm1, xmm2, 0b00001110 // xmm1 = [_, _, 3, 2]
            ; vminps xmm1, xmm1, xmm2 // xmm1 = [_, _, min(3, 1), min(2, 0)]
            ; pshufd Rx(reg(out_reg)), xmm1, 0b00000001 // out = max(3, 1)
            ; minss Rx(reg(out_reg)), xmm1 // out[0] is lowest value

            // Extract the horizontal minimum into xmm2
            ; pshufd xmm1, xmm2, 0b00001110 // xmm1 = [_, _, 3, 2]
            ; vmaxps xmm1, xmm1, xmm2 // xmm1 = [_, _, max(3, 1), max(2, 0)]
            ; pshufd xmm2, xmm1, 0b00000001 // xmm2 = max(3, 1)
            ; maxss xmm2, xmm1 // xmm2[0] is highest value

            // Splice the two together
            // TODO is there a better way to do this?
            ; movd eax, xmm2
            ; shl rax, 32
            ; movd ecx, Rx(reg(out_reg))
            ; or rax, rcx
            ; movq Rx(reg(out_reg)), rax

            ; end:
        );
    }
    fn build_max(&mut self, out_reg: u8, lhs_reg: u8, rhs_reg: u8) {
        dynasm!(self.0.ops
            ; mov ax, [rsi]

            // xmm1 = lhs.upper
            ; pshufd xmm1, Rx(reg(lhs_reg)), 0b11111101u8 as i8
            ; comiss xmm1, Rx(reg(rhs_reg)) // compare lhs.upper and rhs.lower
            ; jp >nan
            ; jb >rhs

            // xmm1 = rhs.upper
            ; pshufd xmm1, Rx(reg(rhs_reg)), 0b11111101u8 as i8
            ; comiss xmm1, Rx(reg(lhs_reg))
            ; jp >nan
            ; jb >lhs

            // Fallthrough: ambiguous case
            ; vmaxps Rx(reg(out_reg)), Rx(reg(lhs_reg)), Rx(reg(rhs_reg))
            ; or ax, CHOICE_BOTH as i16
            ; jmp >end

            ; nan:
            ; or ax, CHOICE_BOTH as i16
            ; mov eax, f32::NAN.to_bits() as i32
            ; movd Rx(reg(out_reg)), eax
            ; vbroadcastss Rx(reg(out_reg)), Rx(reg(out_reg))
            ; jmp >end

            // lhs.upper < rhs.lower
            ; lhs:
            ; movq Rx(reg(out_reg)), Rx(reg(lhs_reg))
            ; or ax, CHOICE_LEFT as i16
            ; mov cx, 1 // TODO: why can't we write 1 to [rdx] directly?
            ; mov [rdx], cx
            ; jmp >end

            // rhs.upper < lhs.lower
            ; rhs:
            ; movq Rx(reg(out_reg)), Rx(reg(rhs_reg))
            ; or ax, CHOICE_RIGHT as i16
            ; mov cx, 1
            ; mov [rdx], cx
            ; jmp >end

            ; end:
            ; mov [rsi], ax
            ; add rsi, 1
        );
    }
    fn build_min(&mut self, out_reg: u8, lhs_reg: u8, rhs_reg: u8) {
        // TODO: Godbolt uses unpcklps ?
        dynasm!(self.0.ops
            //  if lhs.upper < rhs.lower
            //      *choices++ |= CHOICE_LEFT
            //      out = lhs
            //  elif rhs.upper < lhs.lower
            //      *choices++ |= CHOICE_RIGHT
            //      out = rhs
            //  else
            //      *choices++ |= CHOICE_BOTH
            //      out = fmin(lhs, rhs)

            ; mov ax, [rsi]

            // TODO: use cmpltss to do both comparisons?

            // xmm1 = lhs.upper
            ; pshufd xmm1, Rx(reg(lhs_reg)), 0b11111101u8 as i8
            ; comiss xmm1, Rx(reg(rhs_reg)) // compare lhs.upper and rhs.lower
            ; jp >nan
            ; jb >lhs

            // xmm1 = rhs.upper
            ; pshufd xmm1, Rx(reg(rhs_reg)), 0b11111101u8 as i8
            ; comiss xmm1, Rx(reg(lhs_reg))
            ; jp >nan
            ; jb >rhs

            // Fallthrough: ambiguous case
            ; vminps Rx(reg(out_reg)), Rx(reg(lhs_reg)), Rx(reg(rhs_reg))
            ; or ax, CHOICE_BOTH as i16
            ; jmp >end

            ; nan:
            ; or ax, CHOICE_BOTH as i16
            ; mov eax, f32::NAN.to_bits() as i32
            ; movd Rx(reg(out_reg)), eax
            ; vbroadcastss Rx(reg(out_reg)), Rx(reg(out_reg))
            ; jmp >end

            // lhs.upper < rhs.lower
            ; lhs:
            ; movq Rx(reg(out_reg)), Rx(reg(lhs_reg))
            ; or ax, CHOICE_LEFT as i16
            ; mov cx, 1 // TODO: why can't we write 1 to [rdx] directly?
            ; mov [rdx], cx
            ; jmp >end

            // rhs.upper < lhs.lower
            ; rhs:
            ; movq Rx(reg(out_reg)), Rx(reg(rhs_reg))
            ; or ax, CHOICE_RIGHT as i16
            ; mov cx, 1
            ; mov [rdx], cx
            ; jmp >end

            ; end:
            ; mov [rsi], ax
            ; add rsi, 1
        );
    }
    fn load_imm(&mut self, imm: f32) -> u8 {
        let imm_u32 = imm.to_bits();
        dynasm!(self.0.ops
            ; mov eax, imm_u32 as i32
            ; movd Rx(IMM_REG), eax
            ; vbroadcastss Rx(IMM_REG), Rx(IMM_REG)
        );
        IMM_REG.wrapping_sub(OFFSET)
    }
    fn finalize(mut self, out_reg: u8) -> Result<Mmap, Error> {
        dynasm!(self.0.ops
            ; movq xmm0, Rx(reg(out_reg))
            ; add rsp, self.0.mem_offset as i32
            ; pop rbp
            ; ret
        );
        let n = self.0.ops.len();
        let out = self.0.ops.finalize()?;
        Ok(out)
    }
}

pub type JitIntervalEval = JitTracingEval<IntervalAssembler>;
