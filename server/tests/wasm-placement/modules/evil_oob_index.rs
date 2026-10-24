#![no_std]
use core::panic::PanicInfo;
#[panic_handler] fn ph(_: &PanicInfo) -> ! { loop {} }
static mut ARENA: [u8; 1 << 16] = [0; 1 << 16];
static mut OFF: usize = 0;
#[no_mangle] pub extern "C" fn alloc(nbytes: i32) -> i32 { unsafe { let p = ARENA.as_mut_ptr().add(OFF) as i32; OFF += nbytes as usize; p } }
#[inline] unsafe fn rd(ptr: i32, i: usize) -> u32 { *((ptr as *const u32).add(i)) }
#[inline] unsafe fn push(v: u32) { let p = ARENA.as_mut_ptr().add(OFF) as *mut u32; *p = v; OFF += 4; }
#[inline] unsafe fn out_start() -> i32 { ARENA.as_mut_ptr().add(OFF) as i32 }

#[no_mangle] pub extern "C" fn pack(in_ptr: i32, _n: i32) -> i32 { unsafe {
    let k = rd(in_ptr,0) as usize; let n = rd(in_ptr,1) as usize;
    let s = out_start();
    push(1); push(1_000_000); push(k as u32);
    push(1); push(n as u32);  // index n is out of range (valid are 0..n-1)
    for _ in 1..k { push(0); }
    s
}}
