use crate::entrypoint::abort_with_code;

#[no_mangle]
pub extern "C" fn abort() -> ! {
    abort_with_code(0)
}
