#[inline]
pub fn wait_for_interrupt() {
    unsafe { core::arch::asm!("wfi", options(nomem, nostack)) }
}
