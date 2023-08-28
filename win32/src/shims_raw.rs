//! "Shims" are my word for the mechanism for x86 -> retrowin32 (and back) calls.
//!
//! This module implements Shims for non-emulated cpu case, using raw 32-bit memory.
//! See doc/x86-64.md for an overview.

use crate::{ldt::LDT, shims::Shim, Machine};

/// Wraps a region of low (32-bit) memory for us to generate code/etc. into.
struct ScratchSpace {
    ptr: *mut u8,
    len: usize,
    ofs: usize,
}

impl Default for ScratchSpace {
    fn default() -> Self {
        Self {
            ptr: std::ptr::null_mut(),
            len: 0,
            ofs: 0,
        }
    }
}

impl ScratchSpace {
    fn new(ptr: *mut u8, len: usize) -> Self {
        ScratchSpace { ptr, len, ofs: 0 }
    }

    /// Realign current write offset.  This probably doesn't matter but it makes
    /// reading the output a little easier.
    fn realign(&mut self) {
        let align = 8;
        self.ofs = self.ofs + (align - 1) & !(align - 1);
        if self.ofs > self.len {
            panic!("overflow");
        }
    }

    /// Write some data to the scratch space, returning the address it was written to.
    unsafe fn write(&mut self, buf: &[u8]) -> *mut u8 {
        let ptr = self.ptr.add(self.ofs);
        std::ptr::copy_nonoverlapping(buf.as_ptr(), ptr, buf.len());
        self.ofs += buf.len();
        if self.ofs > self.len {
            panic!("overflow");
        }
        ptr
    }
}

pub struct Shims {
    buf: ScratchSpace,
    /// Address that we write a pointer to the Machine to.
    machine_ptr: *mut u8,

    /// Segment selector for 32-bit code.
    code32_selector: u16,

    /// Value for esp in 32-bit mode.
    esp: u32,

    /// Address of the call64 trampoline.
    call64_addr: u32,
    /// Address of the tramp32 trampoline.
    pub tramp32_addr: u32,
}

impl Shims {
    pub fn new(ldt: &mut LDT, addr: *mut u8, size: u32) -> Self {
        // Wine marks all of memory as code.
        let code32_selector = ldt.add_entry(0, 0xFFFF_FFFF, true);

        unsafe {
            let mut buf = ScratchSpace::new(addr, size as usize);

            // trampoline_x86-64.s:call64:
            let call64 = buf.write(b"\x57\x56");
            buf.write(b"\x48\xbf");
            let machine_ptr = buf.write(&0u64.to_le_bytes());
            buf.write(
                b"\x48\x8d\x74\x24\x20\
                \xff\x54\x24\x18\
                \x5e\x5f\
                \xca\x08\x00",
            );
            buf.realign();

            // 16:32 selector:address of call64
            let call64_addr = buf.write(&(call64 as u32).to_le_bytes()) as u32;
            buf.write(&(0x2bu32).to_le_bytes());
            buf.realign();

            // trampoline_x86.s:tramp32:
            let tramp32_addr = buf.write(b"\x89\xfc\xff\xd6\xcb") as u32;
            buf.realign();

            Shims {
                buf,
                machine_ptr,
                esp: 0,
                call64_addr,
                tramp32_addr,
                code32_selector,
            }
        }
    }

    /// HACK: we need a pointer to the Machine, but we get it so late we have to poke it in
    /// way after all the initialization happens...
    pub unsafe fn set_machine_hack(&mut self, machine: *const Machine, esp: u32) {
        let addr = machine as u64;
        std::ptr::copy_nonoverlapping(&addr, self.machine_ptr as *mut u64, 1);
        self.esp = esp;
    }

    pub fn add(&mut self, shim: Shim) -> u32 {
        unsafe {
            let target: u64 = shim.func as u64;

            // trampoline_x86.s:tramp64

            // pushl high 32 bits of dest
            let tramp_addr = self.buf.write(b"\x68") as u32;
            self.buf.write(&((target >> 32) as u32).to_le_bytes());
            // pushl low 32 bits of dest
            self.buf.write(b"\x68");
            self.buf.write(&(target as u32).to_le_bytes());

            // lcalll *call64_addr
            self.buf.write(b"\xff\x1d");
            self.buf.write(&self.call64_addr.to_le_bytes());

            // retl <16-bit bytes to pop>
            self.buf.write(b"\xc2");
            // TODO revisit stack_consumed, does it include eip or not?
            // We have to -4 here to not include IP.
            let stack_consumed: u16 = shim.stack_consumed as u16 - 4;
            self.buf.write(&stack_consumed.to_le_bytes());
            self.buf.realign();

            tramp_addr
        }
    }

    pub fn add_todo(&mut self, _name: String) -> u32 {
        // trampoline_x86.rs:crash
        unsafe { self.buf.write(b"\xcc\xb8\x01\x00\x00\x00\xff\x20") as u32 }
    }
}

/// Synchronously evaluate a Future, under the assumption that it is always immediately Ready.
#[allow(deref_nullptr)]
pub fn call_sync<T>(future: std::pin::Pin<&mut impl std::future::Future<Output = T>>) -> T {
    let context: &mut std::task::Context = unsafe { &mut *std::ptr::null_mut() };
    match future.poll(context) {
        std::task::Poll::Pending => unreachable!(),
        std::task::Poll::Ready(t) => t,
    }
}

pub struct UnimplFuture {}
impl std::future::Future for UnimplFuture {
    type Output = ();

    fn poll(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::task::Poll::Ready(())
    }
}

pub fn call_x86(machine: &mut Machine, func: u32, args: Vec<u32>) -> UnimplFuture {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        // To jump between 64/32 we need to stash some m16:32 pointers, and in particular to
        // be able to return to our 64-bit RIP we want to use a lcall/lret pair.
        //
        // So we lay out the 32-bit stack like this before going into assembly:
        //   arg0
        //   ...
        //   argN
        //   [8 bytes space for m16:32]
        //   [8 bytes space for rsp]  <- lcall_esp
        //
        // The asm then backs up $rsp in the bottom slot,
        // then lcall tramp32 (which pushes m16:32 in the second slot),
        // and then tramp32 switches esp to point to the top of this stack.
        // When tramp32 returns it pops the m16:32, and this code pops rsp.

        let mem = machine.memory.mem();
        let orig_esp = machine.shims.esp;

        // TODO: align?
        machine.shims.esp -= 8; // space for rsp
        let lcall_esp = machine.shims.esp;
        machine.shims.esp -= 8; // space for m16:32 return address pushed by lcall
        for &arg in args.iter().rev() {
            machine.shims.esp -= 4;
            mem.put::<u32>(machine.shims.esp, arg);
        }

        let m1632: u64 =
            ((machine.shims.code32_selector as u64) << 32) | machine.shims.tramp32_addr as u64;

        std::arch::asm!(
            "movq %rsp, ({lcall_esp:r})", // save 64-bit stack
            "movl {lcall_esp:e}, %esp",   // switch to 32-bit stack
            "lcalll *({m1632})",          // jump to 32-bit code
            "popq %rsp",                  // restore 64-bit stack
            options(att_syntax),
            lcall_esp = in(reg) lcall_esp,
            m1632 = in(reg) &m1632,
            inout("edi") machine.shims.esp => _,  // tramp32: new stack
            inout("esi") func => _,  // tramp32: address to call
            // TODO: more clobbers?
        );
        println!("call_x86 done {:x}", func);
        machine.shims.esp = orig_esp;
        UnimplFuture {}
    }

    #[cfg(not(target_arch = "x86_64"))] // just to keep editor from getting confused
    {
        _ = machine.shims.code32_selector;
        _ = machine;
        _ = func;
        _ = args;
        todo!()
    }
}
