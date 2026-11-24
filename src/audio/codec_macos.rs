use super::{Pcm, failed};
use crate::{audit::Scope, error::Result};
use std::{
    ffi::c_void,
    ptr::NonNull,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use zeroize::Zeroizing;

unsafe extern "C" {
    fn mc_audio_decoder(out: *mut *mut c_void) -> i32;
    fn mc_audio_decode(
        converter: *mut c_void,
        bytes: *const u8,
        size: u32,
        samples: *mut i16,
        frames: *mut u32,
    ) -> i32;
    fn AudioConverterDispose(converter: *mut c_void) -> i32;
}

pub struct SystemCodec;

pub fn codec_available() -> Result<Option<SystemCodec>> {
    let converter = Converter::new()?;
    drop(converter);
    Ok(Some(SystemCodec))
}

struct Converter(NonNull<c_void>);
// The decoder is moved into its worker and accessed only by that worker.
unsafe impl Send for Converter {}
impl Converter {
    fn new() -> Result<Self> {
        let mut raw = std::ptr::null_mut();
        let code = unsafe { mc_audio_decoder(&mut raw) };
        NonNull::new(raw)
            .filter(|_| code == 0)
            .map(Self)
            .ok_or_else(|| {
                failed(format!(
                    "AudioToolbox AAC-ELD decoder is unavailable (OSStatus {code})"
                ))
            })
    }
}
impl Drop for Converter {
    fn drop(&mut self) {
        unsafe {
            AudioConverterDispose(self.0.as_ptr());
        }
    }
}

#[derive(Clone)]
pub(crate) struct DecoderControl(Arc<AtomicBool>);
impl DecoderControl {
    pub fn check(&self) -> Result<()> {
        if self.0.load(Ordering::Acquire) {
            Err(failed("Audio decoder stopped"))
        } else {
            Ok(())
        }
    }
    pub fn stop(&self) -> Result<()> {
        self.0.store(true, Ordering::Release);
        Ok(())
    }
}
pub(crate) struct Decoder {
    converter: Converter,
    control: DecoderControl,
    pending: Vec<Pcm>,
}
impl Decoder {
    pub fn start(_: &SystemCodec, _: Scope) -> Result<Self> {
        Ok(Self {
            converter: Converter::new()?,
            control: DecoderControl(Default::default()),
            pending: vec![],
        })
    }
    pub fn control(&self) -> DecoderControl {
        self.control.clone()
    }
    pub fn feed(&mut self, bytes: &[u8]) -> Result<()> {
        self.control.check()?;
        if bytes.is_empty() || bytes.len() > 4096 {
            return Err(failed("Invalid AAC packet size"));
        }
        let mut samples = Zeroizing::new(vec![0i16; 4096]);
        let capacity = (samples.len() / 2) as u32;
        let mut frames = capacity;
        let code = unsafe {
            mc_audio_decode(
                self.converter.0.as_ptr(),
                bytes.as_ptr(),
                bytes.len() as u32,
                samples.as_mut_ptr(),
                &mut frames,
            )
        };
        if code != 0 || frames > capacity {
            return Err(failed(format!(
                "AAC packet decoding failed (OSStatus {code})"
            )));
        }
        if frames > 0 {
            samples.truncate(frames as usize * 2);
            self.pending.push(Pcm { samples });
        }
        Ok(())
    }
    pub fn frames(&mut self) -> Result<Vec<Pcm>> {
        Ok(std::mem::take(&mut self.pending))
    }
    pub fn close(&mut self) -> Result<()> {
        self.control.stop()
    }
}
