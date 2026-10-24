#![no_std]
use core::panic::PanicInfo;
#[panic_handler] fn ph(_: &PanicInfo) -> ! { loop {} }
static mut ARENA: [u8; 1 << 16] = [0; 1 << 16];
static mut OFF: usize = 0;
#[no_mangle] pub extern "C" fn alloc(nbytes: i32) -> i32 { unsafe { let p = ARENA.as_mut_ptr().add(OFF) as i32; OFF += nbytes as usize; p } }
#[inline] unsafe fn rd(ptr: i32, i: usize) -> u32 { *((ptr as *const u32).add(i)) }
#[inline] unsafe fn push(v: u32) { let p = ARENA.as_mut_ptr().add(OFF) as *mut u32; *p = v; OFF += 4; }
#[inline] unsafe fn out_start() -> i32 { ARENA.as_mut_ptr().add(OFF) as i32 }

#[no_mangle] pub extern "C" fn pack(_i: i32, _n: i32) -> i32 { loop {} }
