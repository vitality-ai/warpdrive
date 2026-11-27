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
    let mut binsum = [0u64; 64];
    let mut i = 0usize; while i < n { binsum[i % k] += rd(in_ptr, 2 + i) as u64; i += 1; }
    let mut cap = 0u64; let mut b = 0usize; while b < k { if binsum[b] > cap { cap = binsum[b]; } b += 1; }
    let s = out_start();
    push(1);               // one stripe
    push(cap as u32);      // capacity = largest bin
    push(k as u32);
    let mut b2 = 0usize;
    while b2 < k {
        let mut cnt = 0usize; let mut t = b2; while t < n { cnt += 1; t += k; }
        push(cnt as u32);
        let mut u = b2; while u < n { push(u as u32); u += k; }
        b2 += 1;
    }
    s
}}
