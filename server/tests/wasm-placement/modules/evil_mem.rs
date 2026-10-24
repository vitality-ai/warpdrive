#![no_std]
use core::panic::PanicInfo;
#[panic_handler] fn ph(_: &PanicInfo) -> ! { loop {} }
static mut ARENA: [u8; 1 << 16] = [0; 1 << 16];
static mut OFF: usize = 0;
#[no_mangle] pub extern "C" fn alloc(nbytes: i32) -> i32 { unsafe { let p = ARENA.as_mut_ptr().add(OFF) as i32; OFF += nbytes as usize; p } }
#[no_mangle] pub extern "C" fn pack(_i: i32, _n: i32) -> i32 {
    unsafe {
        let mut addr: usize = 1 << 16;
        loop {
            // ask for 16 MiB each iteration; once cumulative growth passes the
            // StoreLimits cap this is denied and memory stops growing...
            let _ = core::arch::wasm32::memory_grow(0, 256);
            // ...but we keep writing at ever-higher addresses, so the first
            // write past real memory is an out-of-bounds trap.
            *(addr as *mut u8) = 1;
            addr += 1 << 20;
        }
    }
}
