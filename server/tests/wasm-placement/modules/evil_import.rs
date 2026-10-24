#![no_std]
use core::panic::PanicInfo;
#[panic_handler] fn ph(_: &PanicInfo) -> ! { loop {} }
static mut ARENA: [u8; 1 << 16] = [0; 1 << 16];
static mut OFF: usize = 0;
#[no_mangle] pub extern "C" fn alloc(nbytes: i32) -> i32 { unsafe { let p = ARENA.as_mut_ptr().add(OFF) as i32; OFF += nbytes as usize; p } }
// A real wasm import from "env" — the host provides NO imports, so
// instantiation must fail. This models a module trying to reach the host
// (syscall/clock/network/exfil).
#[link(wasm_import_module = "env")]
extern "C" { fn host_secret() -> i32; }
#[no_mangle] pub extern "C" fn pack(_i: i32, _n: i32) -> i32 { unsafe { host_secret() } }
