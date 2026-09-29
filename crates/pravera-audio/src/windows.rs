//! WASAPI: the loopback tap on the host, the render stream on the client.
//!
//! ## Why loopback capture polls
//!
//! Every other WASAPI stream can be event-driven — you hand the client an
//! event handle and it signals once a period. Loopback capture cannot:
//! `Initialize` accepts `AUDCLNT_STREAMFLAGS_EVENTCALLBACK` alongside
//! `AUDCLNT_STREAMFLAGS_LOOPBACK` and then never signals it, because the
//! loopback tap has no timer of its own — it produces data only while some
//! *other* process is rendering. So this polls, at a fraction of the packet
//! length, which costs a couple of hundred wakeups a second and nothing else.
//!
//! That same fact explains a behaviour worth knowing about: **a silent host
//! sends nothing at all**. WASAPI does not manufacture silent packets when no
//! application is playing, so an idle machine costs zero bandwidth and the
//! client's buffer simply runs dry. The client fills the gap with silence,
//! which is what a person expects to hear from a machine playing nothing.
//!
//! ## Why the render stream converts and the capture stream does not
//!
//! `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM` lets a render stream hand WASAPI a
//! format the device does not natively take and have the audio engine resample
//! it. That is exactly what the client needs, and it is Microsoft's resampler
//! rather than one written here. It is documented for render streams; a
//! loopback capture stream is pinned to the mix format whatever you ask for,
//! which is why [`crate::convert`] exists at all.

use std::ptr;
use std::slice;
use std::time::Duration;

use pravera_core::audio::AudioFormat;
use tracing::debug;
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDevice,
    IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVE_FORMAT_PCM,
};
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
};

use crate::{DeviceFormat, Error, Loopback, SampleType, Speaker};

/// How long a WASAPI buffer is asked to be, in 100-nanosecond units.
///
/// 100 ms on the capture side. Nothing is added to latency by a large capture
/// buffer — the tap is drained as fast as the loop runs, and the size only
/// bounds how much can be lost if a scheduling hiccup delays a poll.
const CAPTURE_BUFFER: i64 = 100 * 10_000;

/// The render buffer, in 100-nanosecond units.
///
/// 40 ms. This one *is* latency: audio written here is heard that much later
/// at worst. Small enough to stay in step with video, large enough that a
/// missed wakeup on a busy client does not produce a gap.
const RENDER_BUFFER: i64 = 40 * 10_000;

/// How long a poll of the loopback tap sleeps when there was nothing to read.
///
/// A quarter of a packet, so a packet is never delayed by more than a fraction
/// of its own length by the polling itself.
const POLL: Duration = Duration::from_micros(1_250);

/// Join the multi-threaded apartment.
///
/// Every thread touching these interfaces must be in one. `S_FALSE` means this
/// thread was already initialised, and `RPC_E_CHANGED_MODE` means it is in a
/// single-threaded apartment somebody else set up — neither is a failure, and
/// both leave the thread able to use in-process objects.
fn enter_apartment() {
    // Safety: no arguments, no aliasing, and the result is only inspected.
    let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if result.is_err() {
        debug!(?result, "COM was already initialised on this thread");
    }
}

/// The default rendering endpoint — the speakers, whatever they are today.
fn default_output() -> Result<IMMDevice, Error> {
    // Safety: `MMDeviceEnumerator` is an in-process COM class; the call
    // returns an owned interface or an error, and never borrows the arguments.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(refused)?;
        enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(refused)
    }
}

/// Turn a COM failure into ours, keeping the platform's own wording.
///
/// The message reaches the host's log, never a peer: it can name drivers and
/// devices, and [`crate::Error::Device`] is the variant that never crosses
/// the wire.
fn refused(error: windows::core::Error) -> Error {
    Error::Device(error.message())
}

/// Work out what a `WAVEFORMATEX` is actually describing.
///
/// The mix format is essentially always `WAVE_FORMAT_EXTENSIBLE` wrapping
/// 32-bit float, but the older tags still turn up on virtual devices and on
/// machines where somebody has forced a format, so all of them are read.
///
/// # Safety
///
/// `format` must point at a valid `WAVEFORMATEX`, and — when its tag is
/// `WAVE_FORMAT_EXTENSIBLE` — at a full `WAVEFORMATEXTENSIBLE`. WASAPI
/// guarantees both for anything `GetMixFormat` returns.
unsafe fn read_format(format: *const WAVEFORMATEX) -> Result<DeviceFormat, Error> {
    // Safety: the caller guarantees the pointer. Fields are read by value out
    // of a packed struct, so each is copied rather than referenced.
    let (tag, channels, rate, bits) = unsafe {
        (
            (*format).wFormatTag as u32,
            (*format).nChannels,
            (*format).nSamplesPerSec,
            (*format).wBitsPerSample,
        )
    };

    let subformat = if tag == WAVE_FORMAT_EXTENSIBLE {
        // Safety: the tag is what promises the larger struct is there.
        Some(unsafe { (*format.cast::<WAVEFORMATEXTENSIBLE>()).SubFormat })
    } else {
        None
    };

    let float = match subformat {
        Some(guid) if guid == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT => true,
        Some(guid) if guid == KSDATAFORMAT_SUBTYPE_PCM => false,
        // 3 is WAVE_FORMAT_IEEE_FLOAT, the pre-extensible spelling.
        Some(_) => return Err(Error::Device("unrecognised sample format".into())),
        None => tag == 3,
    };

    let sample_type = match (float, bits) {
        (true, 32) => SampleType::F32,
        (false, 16) => SampleType::I16,
        (false, 24) => SampleType::I24,
        (false, 32) => SampleType::I32,
        _ => return Err(Error::Device(format!("{bits}-bit audio is not supported"))),
    };

    let format = DeviceFormat {
        sample_rate: rate,
        channels,
        sample_type,
    };
    if !format.is_usable() {
        return Err(Error::Device(format!("the device reports {format}")));
    }
    Ok(format)
}

// ---------------------------------------------------------------- loopback

/// A tap on the audio engine's output mix.
struct WasapiLoopback {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    format: DeviceFormat,
    frame_bytes: usize,
}

// Safety: every method takes `&mut self`, so no two threads touch the
// interfaces at once, and both this object and the thread that uses it live in
// the multi-threaded apartment — where in-process interface pointers may
// legitimately move between threads. It is built on the thread that drives it
// in any case; this bound exists so it can be boxed into a `Send` trait
// object alongside the rest of the pipeline.
unsafe impl Send for WasapiLoopback {}

fn open_loopback_inner() -> Result<WasapiLoopback, Error> {
    enter_apartment();
    let device = default_output()?;

    // Safety: `device` is a live endpoint; `Activate` returns an owned
    // interface. `GetMixFormat` allocates a format the client then reads.
    unsafe {
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(refused)?;

        // `GetMixFormat` allocates with the COM task allocator and hands
        // ownership over. Read it, initialise from it, then give it back —
        // a session that reconnects a hundred times should not leak a hundred
        // format blocks.
        let mix = client.GetMixFormat().map_err(refused)?;
        let described = read_format(mix);
        let started = described.as_ref().ok().map(|_| {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                CAPTURE_BUFFER,
                0,
                mix,
                None,
            )
        });
        CoTaskMemFree(Some(mix.cast()));

        let format = described?;
        started
            .expect("a described format was initialised")
            .map_err(refused)?;

        let capture: IAudioCaptureClient = client.GetService().map_err(refused)?;
        client.Start().map_err(refused)?;

        debug!(%format, "tapped the output mix");
        Ok(WasapiLoopback {
            client,
            capture,
            format,
            frame_bytes: format.frame_bytes(),
        })
    }
}

impl Loopback for WasapiLoopback {
    fn format(&self) -> DeviceFormat {
        self.format
    }

    fn read(&mut self, out: &mut Vec<u8>) -> Result<usize, Error> {
        let before = out.len();

        loop {
            // Safety: the capture client is live for as long as `self` is, and
            // every buffer taken here is released before the next iteration.
            let available = unsafe { self.capture.GetNextPacketSize() }.map_err(refused)?;
            if available == 0 {
                break;
            }

            let mut data: *mut u8 = ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;

            // Safety: the three out-parameters are distinct locals, and the
            // two optional timestamps are declined.
            unsafe {
                self.capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                    .map_err(refused)?;
            }

            let bytes = frames as usize * self.frame_bytes;
            if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                // WASAPI is allowed to signal silence without providing the
                // zeroes. Writing them out keeps the stream continuous, which
                // is what stops the client's clock drifting through a pause.
                out.resize(out.len() + bytes, 0);
            } else {
                // Safety: WASAPI guarantees `frames` frames at `data` until
                // `ReleaseBuffer`, and `frame_bytes` came from the same format.
                out.extend_from_slice(unsafe { slice::from_raw_parts(data, bytes) });
            }

            // Safety: releases exactly what was taken, as WASAPI requires.
            unsafe { self.capture.ReleaseBuffer(frames) }.map_err(refused)?;
        }

        Ok(out.len() - before)
    }

    fn wait(&mut self, timeout: Duration) -> bool {
        std::thread::sleep(POLL.min(timeout));
        // Always "go and look": the tap has no event to signal, so the poll
        // interval *is* the signal.
        true
    }

    fn name(&self) -> &str {
        "WASAPI loopback"
    }
}

impl Drop for WasapiLoopback {
    fn drop(&mut self) {
        // Safety: stopping a started client is always valid, and the result is
        // uninteresting — the object is going away either way.
        let _ = unsafe { self.client.Stop() };
    }
}

pub fn open_loopback() -> Result<Box<dyn Loopback>, Error> {
    Ok(Box::new(open_loopback_inner()?))
}

// ----------------------------------------------------------------- speaker

struct WasapiSpeaker {
    client: IAudioClient,
    render: IAudioRenderClient,
    /// Frames the device buffer holds in total, from `GetBufferSize`.
    capacity: u32,
    channels: usize,
    started: bool,
}

// Safety: as for `WasapiLoopback` — `&mut self` throughout, multi-threaded
// apartment, and built on the thread that drives it.
unsafe impl Send for WasapiSpeaker {}

fn open_speaker_inner(format: AudioFormat) -> Result<WasapiSpeaker, Error> {
    if !format.is_playable() {
        return Err(Error::Device(
            "the session named a format nobody can play".into(),
        ));
    }

    enter_apartment();
    let device = default_output()?;

    let channels = format.channels as u16;
    let block = channels * 2;
    let wanted = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: channels,
        nSamplesPerSec: format.sample_rate,
        nAvgBytesPerSec: format.sample_rate * block as u32,
        nBlockAlign: block,
        wBitsPerSample: 16,
        cbSize: 0,
    };

    // Safety: `wanted` outlives the `Initialize` call, which copies it.
    unsafe {
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(refused)?;
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                // Let the audio engine do the conversion to whatever the
                // device actually takes. Its resampler is better than the one
                // in `convert`, and this is the side that gets to use it.
                AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                RENDER_BUFFER,
                0,
                &wanted,
                None,
            )
            .map_err(refused)?;

        let capacity = client.GetBufferSize().map_err(refused)?;
        let render: IAudioRenderClient = client.GetService().map_err(refused)?;

        debug!(
            capacity,
            rate = format.sample_rate,
            channels,
            "opened the speakers"
        );
        Ok(WasapiSpeaker {
            client,
            render,
            capacity,
            channels: channels as usize,
            started: false,
        })
    }
}

impl Speaker for WasapiSpeaker {
    fn space(&mut self) -> Result<usize, Error> {
        Ok(self.capacity.saturating_sub(self.padding()?) as usize)
    }

    fn write(&mut self, samples: &[i16]) -> Result<usize, Error> {
        if self.channels == 0 {
            return Ok(0);
        }

        let offered = samples.len() / self.channels;
        let frames = offered.min(self.space()?) as u32;
        if frames == 0 {
            return Ok(0);
        }

        // Safety: `GetBuffer` hands back `frames` frames of writable memory,
        // valid until `ReleaseBuffer`. The slice below is exactly that long,
        // and the copy source is at least as long because `frames` was
        // clamped to the samples on offer.
        unsafe {
            let target = self.render.GetBuffer(frames).map_err(refused)?;
            let count = frames as usize * self.channels;
            let destination = slice::from_raw_parts_mut(target.cast::<i16>(), count);
            destination.copy_from_slice(&samples[..count]);
            self.render.ReleaseBuffer(frames, 0).map_err(refused)?;
        }

        // Started only once there is something to play, so the device does not
        // render the empty buffer as a burst of silence before the first
        // packet arrives.
        if !self.started {
            // Safety: starting a client that has been initialised and never
            // started is exactly what this call is for.
            unsafe { self.client.Start() }.map_err(refused)?;
            self.started = true;
        }

        Ok(frames as usize)
    }

    fn queued(&mut self) -> Result<usize, Error> {
        Ok(self.padding()? as usize)
    }

    fn wait(&mut self, timeout: Duration) -> bool {
        std::thread::sleep(POLL.min(timeout));
        true
    }

    fn name(&self) -> &str {
        "WASAPI"
    }
}

impl WasapiSpeaker {
    fn padding(&mut self) -> Result<u32, Error> {
        // Safety: valid on an initialised client whether or not it is started.
        unsafe { self.client.GetCurrentPadding() }.map_err(refused)
    }
}

impl Drop for WasapiSpeaker {
    fn drop(&mut self) {
        if self.started {
            // Safety: stopping a started client is always valid.
            let _ = unsafe { self.client.Stop() };
        }
    }
}

pub fn open_speaker(format: AudioFormat) -> Result<Box<dyn Speaker>, Error> {
    Ok(Box::new(open_speaker_inner(format)?))
}
