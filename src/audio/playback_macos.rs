use super::*;
use std::{ffi::c_void, time::Duration};

unsafe extern "C" {
    fn mc_audio_output(
        context: *const c_void,
        fill: unsafe extern "C" fn(*const c_void, *mut u8, u32) -> bool,
        out: *mut *mut c_void,
    ) -> i32;
    fn mc_audio_output_error(output: *mut c_void) -> i32;
    fn mc_audio_output_close(output: *mut c_void);
}

unsafe extern "C" fn fill(shared: *const c_void, dst: *mut u8, frames: u32) -> bool {
    let shared = unsafe { &*shared.cast::<Shared>() };
    if shared.stopping.load(Ordering::Acquire) {
        return false;
    }
    let block = shared
        .buffer
        .lock()
        .ok()
        .and_then(|mut b| b.render(frames as usize).ok());
    let Some(block) = block else {
        shared.failed.store(true, Ordering::Release);
        return false;
    };
    unsafe {
        std::ptr::copy_nonoverlapping(block.bytes.as_ptr(), dst, block.bytes.len());
    }
    shared
        .rendered
        .fetch_add(block.source_frames.into(), Ordering::Relaxed);
    true
}

pub(super) fn run(shared: &Arc<Shared>, scope: &Scope, status: &Sender<Status>) -> Result<()> {
    let buffer = shared
        .buffer
        .lock()
        .map_err(|_| failed("Audio buffer is unavailable"))?;
    let guard = shared
        .changed
        .wait_while(buffer, |b| {
            !b.received && !shared.stopping.load(Ordering::Acquire)
        })
        .map_err(|_| failed("Audio buffer is unavailable"))?;
    drop(guard);
    if shared.stopping.load(Ordering::Acquire) {
        return Ok(());
    }
    let mut output = std::ptr::null_mut();
    let code = unsafe { mc_audio_output(Arc::as_ptr(shared).cast(), fill, &mut output) };
    if code != 0 {
        return Err(failed(format!(
            "Core Audio output could not start (OSStatus {code})"
        )));
    }
    let result = (|| {
        scope.event(
            "media.audio.output.started",
            Level::Info,
            json!({"audio.backend":"core_audio"}),
        )?;
        let _ = status.send(Status::Playing);
        while !shared.stopping.load(Ordering::Acquire) {
            let code = unsafe { mc_audio_output_error(output) };
            if code != 0 || shared.failed.load(Ordering::Acquire) {
                return Err(failed(format!(
                    "Core Audio output failed (OSStatus {code})"
                )));
            }
            thread::sleep(Duration::from_millis(25));
        }
        Ok(())
    })();
    // Synchronous disposal completes callbacks while the Shared Arc is still alive.
    unsafe {
        mc_audio_output_close(output);
    }
    result
}
