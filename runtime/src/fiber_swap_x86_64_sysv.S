/* neon_ctx_swap — a cooperative context switch for x86-64 System V.
 *
 *   void neon_ctx_swap(void** from_sp, void* to_sp);
 *          rdi = from_sp        rsi = to_sp
 *
 * Save this context's callee-saved registers and FP control words onto the current
 * stack, write the resulting stack pointer through `from_sp`, load `to_sp`, and restore
 * the target's saved state. The `ret` then returns into whatever the target last saved:
 * either a previous neon_ctx_swap call site (an ordinary suspend/resume) or a frame
 * primed by neon_fiber_new (a fresh fiber's first run).
 *
 * Callee-saved per the ABI: rbx, rbp, r12-r15, and the control bits of MXCSR and the x87
 * control word. Everything else is caller-saved and need not be preserved across the call.
 *
 * The saved frame, low address to high (this is exactly what neon_fiber_new primes for a
 * fresh stack):
 *     to_sp+0x00  MXCSR (4 bytes) + x87 CW (2 bytes), in a 16-byte slot
 *     to_sp+0x10  r15
 *     to_sp+0x18  r14
 *     to_sp+0x20  r13
 *     to_sp+0x28  r12
 *     to_sp+0x30  rbx
 *     to_sp+0x38  rbp
 *     to_sp+0x40  return address
 * After the pops and `ret`, rsp = to_sp + 0x48.
 */

.text
.globl neon_ctx_swap
.hidden neon_ctx_swap
.type neon_ctx_swap,@function
.align 16
neon_ctx_swap:
    pushq %rbp
    pushq %rbx
    pushq %r12
    pushq %r13
    pushq %r14
    pushq %r15
    subq  $16, %rsp
    stmxcsr (%rsp)          /* save MMX control- and status-word */
    fnstcw  8(%rsp)         /* save x87 control-word */

    movq  %rsp, (%rdi)      /* *from_sp = current rsp (this context is now suspended) */
    movq  %rsi, %rsp        /* rsp = to_sp (switch stacks) */

    ldmxcsr (%rsp)          /* restore MMX control- and status-word */
    fldcw   8(%rsp)         /* restore x87 control-word */
    addq  $16, %rsp
    popq  %r15
    popq  %r14
    popq  %r13
    popq  %r12
    popq  %rbx
    popq  %rbp
    ret
.size neon_ctx_swap,.-neon_ctx_swap

/* No executable stack is required. */
.section .note.GNU-stack,"",%progbits
