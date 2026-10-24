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
    let num = (n + 1) / 2;
    push(num as u32);
    let mut j = 0usize;
    while j < num {
        let i0 = 2*j; let i1 = 2*j + 1;
        let s0 = rd(in_ptr, 2 + i0);
        let (has1, s1) = if i1 < n { (true, rd(in_ptr, 2 + i1)) } else { (false, 0u32) };
        let cap = if s1 > s0 { s1 } else { s0 };
        push(cap); push(k as u32);
        push(1); push(i0 as u32);                 // bin0 = [i0]
        if has1 { push(1); push(i1 as u32); } else { push(0); }   // bin1
        let mut b = 2; while b < k { push(0); b += 1; }           // bins 2..k empty
        j += 1;
    }
    s
}}
