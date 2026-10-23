//! Architecture-specific signal registers and saved return addresses.
//!
//! Called from the sampler's signal handler: no allocation, TLS, locks, loader
//! calls, or reads through application pointers. Frame records on both targets
//! contain two 64-bit words: previous FP, then saved return address.

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Registers {
    pub pc: usize,
    pub sp: usize,
    pub fp: usize,
}

/// The caller must supply the live context passed to an SA_SIGINFO handler.
pub(super) unsafe fn registers(context: *const libc::ucontext_t) -> Registers {
    // SAFETY: the handler's context is valid for the duration of this call.
    let machine = unsafe { &(*context).uc_mcontext };
    #[cfg(target_arch = "x86_64")]
    {
        Registers {
            pc: machine.gregs[libc::REG_RIP as usize] as usize,
            sp: machine.gregs[libc::REG_RSP as usize] as usize,
            fp: machine.gregs[libc::REG_RBP as usize] as usize,
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        Registers {
            pc: machine.pc as usize,
            sp: machine.sp as usize,
            fp: machine.regs[29] as usize,
        }
        // Do not append live x30: after a nested call it can point inside the
        // interrupted function. Without unwind metadata it cannot reliably
        // distinguish a frameless leaf or an interrupted prologue/epilogue.
    }
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn strip_instruction_pac(mut address: usize) -> usize {
    // XPACLRI is HINT #7: it strips an instruction PAC from x30 on capable CPUs
    // and is a NOP on older ones. Unlike a hard-coded address mask this respects
    // the CPU's configured VA width. x30 is an explicit clobber, so LLVM preserves
    // our own return address. This does not authenticate or dereference address.
    // SAFETY: register-only baseline-compatible instruction; no memory access.
    unsafe {
        std::arch::asm!(
            "hint #7",
            inout("x30") address,
            options(nomem, nostack, preserves_flags),
        );
    }
    address
}

/// Map a saved return address into its calling instruction. Interrupted PCs
/// already identify the current instruction and must not receive this bias.
#[inline(always)]
pub(super) fn callsite(address: usize) -> Option<usize> {
    #[cfg(target_arch = "x86_64")]
    {
        address.checked_sub(1)
    }
    #[cfg(target_arch = "aarch64")]
    {
        let address = strip_instruction_pac(address);
        // A64 instructions are four bytes and instruction addresses untagged.
        // Reject misaligned saved links rather than emitting invented frames.
        if !address.is_multiple_of(4) {
            return None;
        }
        address.checked_sub(4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_interrupted_registers_without_biasing_the_pc() {
        // SAFETY: zero is valid for the integer/pointer fields of this fixture;
        // it is never passed to the OS as an actual execution context.
        let mut context: libc::ucontext_t = unsafe { std::mem::zeroed() };
        #[cfg(target_arch = "x86_64")]
        {
            context.uc_mcontext.gregs[libc::REG_RIP as usize] = 0x1234;
            context.uc_mcontext.gregs[libc::REG_RSP as usize] = 0x8000;
            context.uc_mcontext.gregs[libc::REG_RBP as usize] = 0x8100;
        }
        #[cfg(target_arch = "aarch64")]
        {
            context.uc_mcontext.pc = 0x1234;
            context.uc_mcontext.sp = 0x8000;
            context.uc_mcontext.regs[29] = 0x8100;
            // This could be a nested call inside the current function, not
            // the caller. It must not replace either PC or the frame chain.
            context.uc_mcontext.regs[30] = 0x1240;
        }
        assert_eq!(
            unsafe { registers(&context) },
            Registers {
                pc: 0x1234,
                sp: 0x8000,
                fp: 0x8100
            }
        );
    }

    #[test]
    fn return_address_bias_does_not_underflow() {
        assert_eq!(callsite(0), None);
        #[cfg(target_arch = "x86_64")]
        assert_eq!(callsite(0x1234), Some(0x1233));
        #[cfg(target_arch = "aarch64")]
        {
            assert_eq!(callsite(0x1234), Some(0x1230));
            for invalid in [1, 2, 3, 0x1235, 0x1236, 0x1237] {
                assert_eq!(callsite(invalid), None);
            }
        }
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn signed_and_unsigned_return_addresses_resolve_to_the_same_callsite() {
        let base = callsite as *const () as usize;
        // PACIASP, also a baseline-compatible HINT, signs with the current SP.
        // Stripping needs no key or original SP and also works with key B.
        let (mut changed_a, mut changed_b) = (false, false);
        // A valid PAC can happen to be all-zero. Several distinct inputs avoid
        // mistaking that one signature for hardware without authentication.
        for offset in 0..16 {
            let address = base + offset * 4;
            assert_eq!(strip_instruction_pac(address), address);
            let mut signed_a = address;
            let mut signed_b = address;
            unsafe {
                std::arch::asm!("hint #25", inout("x30") signed_a, options(nomem, nostack, preserves_flags));
                std::arch::asm!("hint #27", inout("x30") signed_b, options(nomem, nostack, preserves_flags));
            }
            assert_eq!(callsite(signed_a), callsite(address));
            assert_eq!(callsite(signed_b), callsite(address));
            changed_a |= signed_a != address;
            changed_b |= signed_b != address;
        }
        // Report whether this exercised PAC hardware or backwards-compatible
        // NOPs, so testing without PAC does not claim PAC-hardware coverage.
        println!("PAC signatures changed addresses: key A={changed_a}, key B={changed_b}");
    }
}
