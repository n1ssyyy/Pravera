//! Hardware H.264, through Media Foundation.
//!
//! ## Why this exists
//!
//! The software encoder costs about 26 ms on a 1080p frame on a fast desktop
//! and two to three times that on a laptop. Every stage of the host pipeline
//! runs on one thread, so that figure *is* the frame rate — around eleven
//! frames a second on the machine that prompted this, with the network barely
//! used. A GPU encoder does the same frame in single-digit milliseconds because
//! it is fixed-function silicon rather than a loop over macroblocks.
//!
//! Media Foundation rather than NVENC directly: one code path covers NVIDIA,
//! Intel Quick Sync and AMD, and the machine that has none of them still gets a
//! Microsoft software MFT rather than an error.
//!
//! ## Baseline profile, deliberately
//!
//! The output is Constrained Baseline. That is not the best H.264 available —
//! Main and High compress better at the same quality — and it is chosen because
//! the *decoder* on the far end is openh264, which implements Constrained
//! Baseline and nothing else. A High-profile stream would encode beautifully
//! here and produce a black window there.
//!
//! ## Asynchronous transforms
//!
//! Hardware encoders are async MFTs: they do not answer `ProcessInput` with a
//! frame, they raise `METransformNeedInput` when they want one and
//! `METransformHaveOutput` when they have produced one, and the two are not in
//! step. A frame goes in and its compressed form comes out some number of
//! frames later.
//!
//! [`Encoder::encode`] therefore returns `Ok(None)` for the first few frames of
//! a session and then settles into one frame out per frame in. That is what the
//! `Option` in [`VideoEncoder::encode`] is for, and the host already treats it
//! as ordinary rather than as a failure.
//!
//! Output order matches input order because B-frames are switched off — the
//! Latency profile could not afford their reordering delay anyway — so the
//! capture timestamps ride along in a plain queue rather than needing to be
//! matched up by presentation time.

use std::collections::VecDeque;
use std::sync::Once;

use bytes::Bytes;
use pravera_core::{Codec, PixelFormat, QualityProfile, Resolution};
use tracing::{debug, trace};
use windows::core::{Interface, GUID};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use crate::{CodecError, EncodedFrame, EncoderSettings, RawFrame, Result, VideoEncoder};

/// Units of `IMFSample` timestamps: hundreds of nanoseconds.
const TICKS_PER_SECOND: i64 = 10_000_000;

/// How many events to pump before deciding the encoder has nothing to say.
///
/// A bound rather than a `loop`, because every iteration is a blocking call
/// into a driver and a transform that has wedged should stall one frame rather
/// than the host.
const MAX_EVENTS_PER_FRAME: usize = 16;

/// How many frames a forced keyframe has to appear within.
///
/// Generous enough to cover an encoder's pipeline depth — an asynchronous
/// transform holds several frames in flight, so the answer to a request made
/// now arrives a few frames later — and short enough that a client staring at a
/// frozen picture is not staring at it for long. At sixty frames a second this
/// is a fifth of a second.
const FORCE_GRACE: i64 = 12;

/// Start Media Foundation once for the process.
///
/// Both halves are per-process rather than per-encoder, and calling `MFStartup`
/// twice is legal but pointless. There is no matching `MFShutdown`: it would
/// have to run after every encoder anywhere in the process had been dropped,
/// and getting that wrong tears the platform down under a live session. The
/// operating system reclaims all of it when the process exits.
fn start_platform() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // Multithreaded apartment: the encoder is built on whichever thread
        // asked for it and then used on the host's pipeline thread, which a
        // single-threaded apartment would forbid.
        //
        // SAFETY: both calls are the documented process-wide initialisers and
        // take no pointers from us.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let _ = MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET);
        }
    });
}

/// Pack a width and height the way Media Foundation stores them.
///
/// `MFSetAttributeSize` is an inline helper in the C headers, so there is
/// nothing to call: the two `u32`s go into one `u64`, high half first.
const fn packed(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | low as u64
}

/// Whether this machine can encode H.264 in hardware.
///
/// Answered by asking Media Foundation for the transforms that take NV12 and
/// produce H.264, restricted to hardware ones. Enumerating is cheap and it is
/// the only honest way to answer — a GPU's presence says nothing about whether
/// its encoder is exposed, and a machine in a virtual desktop often has the
/// card and not the encoder.
pub(crate) fn hardware_h264_available() -> bool {
    start_platform();
    match transforms() {
        Ok(found) if found.is_empty() => return false,
        Ok(_) => {}
        Err(error) => {
            debug!(%error, "no hardware H.264 encoder");
            return false;
        }
    }

    // A transform existing is not a transform this machine can use. Building
    // one for real is what settles whether the driver will be pinned to
    // Baseline, and being wrong about that is uniquely expensive: it cannot be
    // detected from this side at all, and the person who finds out is somebody
    // on another machine looking at a black window with no error on it. So the
    // probe pays for one throwaway encoder rather than betting a session.
    //
    // 640x360 because the answer does not depend on the size, and a small one
    // is cheap. A driver that refuses this exact shape and accepts a display's
    // is not a driver worth the risk.
    let settings = crate::EncoderSettings::new(
        Resolution::new(640, 360),
        pravera_core::QualityProfile::Adaptive,
    );
    match HardwareEncoder::new(&crate::EncoderSettings {
        codec: Codec::H264,
        ..settings
    }) {
        Ok(_) => true,
        Err(error) => {
            tracing::warn!(
                %error,
                "this machine has a hardware H.264 encoder but it cannot be used; \
                 falling back to software so the picture is decodable"
            );
            false
        }
    }
}

/// The hardware H.264 encoders this machine exposes, best first.
fn transforms() -> windows::core::Result<Vec<IMFActivate>> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };

    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;

    // SAFETY: both type descriptions live until the call returns, and the out
    // parameters are the shapes `MFTEnumEx` documents.
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            // `SORTANDFILTER` puts the preferred transform first and drops the
            // ones the system considers unusable.
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut activates,
            &mut count,
        )?;
    }

    if activates.is_null() || count == 0 {
        return Ok(Vec::new());
    }

    // SAFETY: `MFTEnumEx` returned an array of `count` initialised interface
    // pointers allocated with `CoTaskMemAlloc`. Taking each one moves the
    // reference out; the array itself is freed below.
    let found = unsafe {
        let slice = std::slice::from_raw_parts(activates, count as usize);
        let owned: Vec<IMFActivate> = slice.iter().filter_map(|slot| slot.clone()).collect();
        windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _));
        owned
    };

    Ok(found)
}

/// The pieces of a freshly opened transform.
struct Built {
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    codec_api: Option<ICodecAPI>,
}

impl Built {
    fn open(settings: &EncoderSettings, resolution: Resolution) -> Result<Built> {
        start_platform();

        let activate = transforms()
            .map_err(|error| CodecError::Init {
                codec: "media foundation",
                reason: error.message(),
            })?
            .into_iter()
            .next()
            .ok_or(CodecError::Init {
                codec: "media foundation",
                reason: "this machine exposes no hardware H.264 encoder".into(),
            })?;

        // SAFETY: `activate` is a live `IMFActivate` from the enumeration.
        let transform: IMFTransform =
            unsafe { activate.ActivateObject() }.map_err(|error| CodecError::Init {
                codec: "media foundation",
                reason: error.message(),
            })?;

        unlock(&transform)?;
        configure(&transform, settings, resolution)?;

        let events: IMFMediaEventGenerator =
            transform.cast().map_err(|error| CodecError::Init {
                codec: "media foundation",
                reason: format!(
                    "the transform is not an event generator: {}",
                    error.message()
                ),
            })?;

        let codec_api: Option<ICodecAPI> = transform.cast().ok();
        if let Some(api) = &codec_api {
            tune(api, settings);
        }

        Ok(Built {
            transform,
            events,
            codec_api,
        })
    }
}

/// A hardware H.264 encoder.
pub(crate) struct HardwareEncoder {
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    codec_api: Option<ICodecAPI>,
    /// Kept so the transform can be opened again on the same terms.
    settings: EncoderSettings,
    resolution: Resolution,
    /// Frame interval in sample ticks.
    tick: i64,
    /// Frames handed in so far, which is where sample timestamps come from.
    counter: i64,
    /// Capture times of frames the encoder has been given and not yet answered
    /// for. Ordinary FIFO: with B-frames off, output order is input order.
    pending: VecDeque<u32>,
    /// How many frames the transform has asked for and not yet been given.
    ///
    /// This has to be a running count rather than a flag, and getting that
    /// wrong is what a stalled encoder looks like. An asynchronous transform
    /// raises one `METransformNeedInput` per frame it is willing to accept, and
    /// it raises several at the start to fill its pipeline. Reading an event
    /// off the queue *consumes* it: a request that is read and not acted on is
    /// gone, and the transform will not raise it again. Discard enough of them
    /// and it is waiting for a frame it already asked for, which presents as a
    /// session that runs for a second and then freezes on one picture while
    /// everything else — input, the control stream, the statistics — keeps
    /// working perfectly.
    credits: usize,
    /// Frames the transform has produced that the caller has not taken yet.
    ///
    /// Every frame out of an H.264 encoder is referenced by the ones after it,
    /// so none of them may be dropped: throwing one away corrupts the picture
    /// until the next keyframe seconds later. The trait hands back one frame
    /// per call, so anything produced beyond that waits here rather than being
    /// overwritten.
    ready: VecDeque<(Bytes, bool, u32)>,
    /// Set when a keyframe has been asked for and not yet handed to the
    /// transform.
    keyframe_pending: bool,
    /// The frame count at which a keyframe was asked for and has not arrived.
    ///
    /// A keyframe request is not advice. It is a client saying it cannot decode
    /// anything more until it gets one, so every predicted frame sent in the
    /// meantime is discarded at the far end. If the transform ignores the
    /// request — and whether it honours it depends on the vendor — the session
    /// stays frozen on one picture indefinitely while input, the control stream
    /// and every counter carry on working, which is about the most confusing
    /// way for a fault to present. So the request is timed, and an encoder that
    /// does not answer it is replaced by one that will.
    awaiting_keyframe: Option<i64>,
    /// Reused NV12 conversion target.
    nv12: Vec<u8>,
    /// Whether the transform has been told streaming began.
    started: bool,
    /// Every event and outcome seen from the driver, so a transform that goes
    /// quiet can say which half of the conversation stopped.
    seen: Seen,
}

/// What the transform has said and done, counted since it was opened.
#[derive(Debug, Default, Clone, Copy)]
struct Seen {
    need_input: u64,
    have_output: u64,
    /// `MF_E_TRANSFORM_STREAM_CHANGE`, each answered by renegotiating.
    stream_changes: u64,
    /// `ProcessOutput` answered with nothing to take.
    empty_outputs: u64,
    /// Frames offered while the transform had not asked for one.
    refused: u64,
    produced: u64,
}

// SAFETY: every interface here is used from one thread at a time — the encoder
// is owned by the host's pipeline thread and the trait takes `&mut self`. They
// were created in a multithreaded apartment, which is what makes moving them
// between threads legal in the first place.
unsafe impl Send for HardwareEncoder {}

impl HardwareEncoder {
    pub(crate) fn new(settings: &EncoderSettings) -> Result<HardwareEncoder> {
        let resolution = settings.encode_resolution();
        let built = Built::open(settings, resolution)?;
        let fps = settings.fps.max(1) as i64;

        Ok(HardwareEncoder {
            transform: built.transform,
            events: built.events,
            codec_api: built.codec_api,
            settings: settings.clone(),
            resolution,
            tick: TICKS_PER_SECOND / fps,
            counter: 0,
            pending: VecDeque::new(),
            credits: 0,
            ready: VecDeque::new(),
            keyframe_pending: false,
            awaiting_keyframe: None,
            nv12: vec![0; PixelFormat::Nv12.frame_size(resolution)],
            started: false,
            seen: Seen::default(),
        })
    }

    /// Throw the transform away and open a new one.
    ///
    /// The one thing every H.264 encoder does identically is begin with an IDR,
    /// so this is a keyframe that does not depend on any vendor honouring a
    /// request. It is the fallback for [`Self::keyframe_overdue`], not the
    /// normal path: it costs a few milliseconds and loses whatever the old
    /// transform had in flight, which is acceptable exactly once — when the far
    /// end is already showing a frozen picture and the alternative is that it
    /// keeps showing one.
    fn rebuild(&mut self) -> Result<()> {
        let built = Built::open(&self.settings, self.resolution)?;

        self.transform = built.transform;
        self.events = built.events;
        self.codec_api = built.codec_api;
        // Everything the old transform was owed or owed us belongs to an
        // encoder that no longer exists.
        self.credits = 0;
        self.pending.clear();
        self.ready.clear();
        self.started = false;
        self.keyframe_pending = false;
        self.awaiting_keyframe = None;
        Ok(())
    }

    /// Whether a requested keyframe has gone unanswered for long enough that
    /// the encoder is not going to answer it.
    fn keyframe_overdue(&self) -> bool {
        self.awaiting_keyframe
            .is_some_and(|since| self.counter.saturating_sub(since) >= FORCE_GRACE)
    }

    fn begin(&mut self) -> Result<()> {
        if self.started {
            return Ok(());
        }
        // SAFETY: the transform is live and both messages take no parameters.
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .and_then(|()| {
                    self.transform
                        .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                })
        }
        .map_err(|error| CodecError::Encode(error.message()))?;
        self.started = true;
        Ok(())
    }

    /// Answer `MF_E_TRANSFORM_STREAM_CHANGE`: take the output type the
    /// transform now offers and set it, so it releases the frame it is holding.
    ///
    /// Refused if the new type is not Baseline, for the same reason
    /// [`configure`] checks: the far end decodes nothing else, and a stream it
    /// cannot decode is worse than an error the host can fall back from.
    fn accept_output_change(&mut self) -> Result<()> {
        // SAFETY: stream 0 is the only output stream; the type returned is
        // owned here and outlives the call that sets it.
        unsafe {
            let offered = self
                .transform
                .GetOutputAvailableType(0, 0)
                .map_err(|error| CodecError::Encode(error.message()))?;
            if let Ok(profile) = offered.GetUINT32(&MF_MT_MPEG2_PROFILE) {
                if profile != eAVEncH264VProfile_Base.0 as u32 {
                    return Err(CodecError::Encode(format!(
                        "the encoder switched itself to profile {profile}, which the other machine cannot decode"
                    )));
                }
            }
            self.transform
                .SetOutputType(0, &offered, 0)
                .map_err(|error| CodecError::Encode(error.message()))
        }
    }

    /// Wrap one NV12 frame as a sample the transform will accept.
    fn sample(&self, timestamp: i64) -> Result<IMFSample> {
        // SAFETY: the length is the NV12 size for this resolution, which is
        // exactly what `self.nv12` was allocated to hold.
        unsafe {
            let buffer = MFCreateMemoryBuffer(self.nv12.len() as u32)
                .map_err(|error| CodecError::Encode(error.message()))?;

            let mut destination: *mut u8 = std::ptr::null_mut();
            let mut capacity = 0u32;
            buffer
                .Lock(&mut destination, Some(&mut capacity), None)
                .map_err(|error| CodecError::Encode(error.message()))?;

            // The buffer Media Foundation handed back is at least what was
            // asked for; copying the smaller of the two is what keeps this
            // sound if it ever hands back less.
            let take = self.nv12.len().min(capacity as usize);
            std::ptr::copy_nonoverlapping(self.nv12.as_ptr(), destination, take);

            let _ = buffer.Unlock();
            buffer
                .SetCurrentLength(take as u32)
                .map_err(|error| CodecError::Encode(error.message()))?;

            let sample = MFCreateSample().map_err(|error| CodecError::Encode(error.message()))?;
            sample
                .AddBuffer(&buffer)
                .and_then(|()| sample.SetSampleTime(timestamp))
                .and_then(|()| sample.SetSampleDuration(self.tick))
                .map_err(|error| CodecError::Encode(error.message()))?;

            Ok(sample)
        }
    }

    /// Read everything the transform has queued, without blocking.
    ///
    /// Both event kinds are *credits* rather than notifications, and both are
    /// destroyed by being read, so every one that comes off the queue has to be
    /// recorded: a `METransformNeedInput` becomes the right to hand over one
    /// frame, and a `METransformHaveOutput` becomes the obligation to take one.
    /// Reading either and doing nothing with it loses a frame permanently.
    fn pump(&mut self) -> Result<()> {
        for _ in 0..MAX_EVENTS_PER_FRAME {
            // SAFETY: a non-blocking read of the next transform event. The
            // returned event is owned here.
            let Ok(event) = (unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) }) else {
                // Nothing queued.
                return Ok(());
            };

            // SAFETY: the event is live.
            let kind = unsafe { event.GetType() }.unwrap_or(0);

            if kind == METransformNeedInput.0 as u32 {
                self.seen.need_input += 1;
                self.credits += 1;
            } else if kind == METransformHaveOutput.0 as u32 {
                self.seen.have_output += 1;
                if let Some((data, keyframe)) = self.take()? {
                    self.seen.produced += 1;
                    if keyframe {
                        // Whatever was being waited for has arrived.
                        self.awaiting_keyframe = None;
                    }
                    // Output order is input order, because B-frames are off.
                    // An empty queue would mean the transform produced more
                    // frames than it was given, which cannot happen; falling
                    // back to zero rather than panicking keeps a driver that
                    // does something unexpected from taking the session down.
                    let capture_micros = self.pending.pop_front().unwrap_or(0);
                    self.ready.push_back((data, keyframe, capture_micros));
                }
            }
        }

        // The bound was reached with events still waiting. Not fatal — the next
        // call reads them — but it means the transform is running well ahead of
        // the caller, which is worth seeing in a log.
        debug!("the encoder produced more events than one frame's worth");
        Ok(())
    }

    /// Take one compressed frame from the transform, if it has one ready.
    fn take(&mut self) -> Result<Option<(Bytes, bool)>> {
        // SAFETY: the stream info is filled in by the call; stream 0 is the
        // only output stream an H.264 encoder MFT has.
        let info = unsafe { self.transform.GetOutputStreamInfo(0) }
            .map_err(|error| CodecError::Encode(error.message()))?;

        // A transform that allocates its own samples must be handed an empty
        // slot; one that does not needs a buffer of its stated size.
        let provides_samples = info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
            != 0;

        let mut buffers = [MFT_OUTPUT_DATA_BUFFER::default()];
        if !provides_samples {
            // SAFETY: a fresh sample and buffer, owned here until handed over.
            let sample = unsafe {
                let buffer = MFCreateMemoryBuffer(info.cbSize.max(1))
                    .map_err(|error| CodecError::Encode(error.message()))?;
                let sample =
                    MFCreateSample().map_err(|error| CodecError::Encode(error.message()))?;
                sample
                    .AddBuffer(&buffer)
                    .map_err(|error| CodecError::Encode(error.message()))?;
                sample
            };
            buffers[0].pSample = std::mem::ManuallyDrop::new(Some(sample));
        }

        let mut status = 0u32;
        // SAFETY: `buffers` outlives the call and holds one correctly shaped
        // entry, which is what `dwOutputStreamCount = 1` promises.
        let result = unsafe { self.transform.ProcessOutput(0, &mut buffers, &mut status) };

        let produced = claim(&mut buffers[0].pSample);
        // Events attached to the output are ours to release, and leaking one
        // per frame is a leak per frame.
        let _ = claim(&mut buffers[0].pEvents);

        match result {
            Ok(()) => {}
            Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                self.seen.empty_outputs += 1;
                return Ok(None);
            }
            Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                // The encoder renegotiated its own output, and it holds the
                // frame until it is told the new type has been accepted. Not
                // answering is not a lost frame, it is every frame: some
                // drivers (Intel's among them) say this on the first output
                // and then on every one after, and nothing ever comes out.
                // The documented answer is to take the type it now offers and
                // set it; the frame then arrives with the next
                // `METransformHaveOutput`.
                self.seen.stream_changes += 1;
                debug!("the encoder changed its output format; accepting the new one");
                self.accept_output_change()?;
                return Ok(None);
            }
            Err(error) => return Err(CodecError::Encode(error.message())),
        }

        let Some(sample) = produced else {
            self.seen.empty_outputs += 1;
            return Ok(None);
        };

        // `CleanPoint` when the transform sets it, and the bitstream itself
        // when it does not.
        //
        // Trusting the attribute alone is a deadlock, not a missed
        // optimisation. Several hardware transforms never set it, so every
        // frame is reported as predicted — including the IDR frames they
        // genuinely produce when asked. The client drops non-keyframes while
        // it is waiting to resynchronise, so a stream where no frame is ever
        // marked a keyframe is a stream it can never start from: frames
        // arrive, reassemble perfectly, and are discarded a layer before the
        // decoder, forever, with nothing anywhere reporting an error.
        //
        // The bitstream cannot be wrong about this in the way a driver can.
        // SAFETY: the sample is live for the rest of this function.
        let claimed = unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }
            .map(|value| value != 0)
            .unwrap_or(false);

        // SAFETY: `ConvertToContiguousBuffer` returns a buffer valid until it
        // is dropped, and the lock/unlock pair brackets every read from it.
        let data = unsafe {
            let buffer = sample
                .ConvertToContiguousBuffer()
                .map_err(|error| CodecError::Encode(error.message()))?;

            let mut source: *mut u8 = std::ptr::null_mut();
            let mut length = 0u32;
            buffer
                .Lock(&mut source, None, Some(&mut length))
                .map_err(|error| CodecError::Encode(error.message()))?;
            let copied = std::slice::from_raw_parts(source, length as usize).to_vec();
            let _ = buffer.Unlock();
            copied
        };

        if data.is_empty() {
            return Ok(None);
        }
        let keyframe = claimed || starts_a_keyframe(&data);
        Ok(Some((Bytes::from(data), keyframe)))
    }
}

/// Whether this access unit can be decoded without anything before it.
///
/// True for a parameter set or an IDR slice, which is what "keyframe" has to
/// mean to the far end: the frame it can start from. Read straight off the
/// Annex B byte stream, because that is the one description of the frame no
/// driver gets a say in.
///
/// Both start-code lengths are accepted. Encoders mix three- and four-byte
/// codes within a single access unit — typically four before the first NAL and
/// three between the rest — and a scanner that knew only one of them would miss
/// exactly the SPS that proves the point.
fn starts_a_keyframe(data: &[u8]) -> bool {
    /// Sequence parameter set: only ever emitted with a keyframe.
    const SPS: u8 = 7;
    /// A slice of an IDR picture.
    const IDR: u8 = 5;

    // Three bytes of start code and one of header is the shortest thing that
    // could answer the question.
    for window in data.windows(4) {
        if window[0] != 0 || window[1] != 0 || window[2] != 1 {
            continue;
        }
        // The low five bits of the byte after the start code are the NAL unit
        // type. The top bit must be zero in a well-formed stream.
        let header = window[3];
        if header & 0x80 != 0 {
            continue;
        }
        if matches!(header & 0x1f, SPS | IDR) {
            return true;
        }
    }
    false
}

/// Tell an async transform we know it is asynchronous.
///
/// A hardware MFT refuses to work at all until this is set — the flag exists so
/// that a caller written for synchronous transforms cannot accidentally drive
/// one that will never answer `ProcessOutput` directly.
fn unlock(transform: &IMFTransform) -> Result<()> {
    // SAFETY: the transform is live; `GetAttributes` hands back a store that
    // lives as long as it does.
    let attributes = unsafe { transform.GetAttributes() }.map_err(|error| CodecError::Init {
        codec: "media foundation",
        reason: error.message(),
    })?;

    // SAFETY: reading and writing well-known attribute GUIDs on a live store.
    unsafe {
        let is_async = attributes.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0;
        if is_async {
            attributes
                .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                .map_err(|error| CodecError::Init {
                    codec: "media foundation",
                    reason: error.message(),
                })?;
        }
    }
    Ok(())
}

/// Set the output format, then the input format.
///
/// The order is not a style choice. An H.264 encoder cannot describe the
/// uncompressed input it accepts until it knows what it is being asked to
/// produce, and setting the input type first fails with a type-negotiation
/// error on every transform that follows the rules.
fn configure(
    transform: &IMFTransform,
    settings: &EncoderSettings,
    resolution: Resolution,
) -> Result<()> {
    let fps = settings.fps.max(1);

    // SAFETY: every call below takes GUIDs and integers by value or by
    // reference to values that outlive the call.
    unsafe {
        let output = MFCreateMediaType().map_err(init)?;
        output
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(init)?;
        output
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
            .map_err(init)?;
        output
            .SetUINT32(&MF_MT_AVG_BITRATE, settings.bitrate)
            .map_err(init)?;
        output
            .SetUINT64(
                &MF_MT_FRAME_SIZE,
                packed(resolution.width, resolution.height),
            )
            .map_err(init)?;
        output
            .SetUINT64(&MF_MT_FRAME_RATE, packed(fps, 1))
            .map_err(init)?;
        output
            .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, packed(1, 1))
            .map_err(init)?;
        output
            .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(init)?;
        // Constrained Baseline, because openh264 decodes nothing else. See the
        // module header: the alternative is a stream that encodes here and
        // shows a black window on the other machine.
        output
            .SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32)
            .map_err(init)?;
        transform.SetOutputType(0, &output, 0).map_err(init)?;

        // Asking is not getting. A hardware encoder is free to accept the type
        // and then emit High profile regardless, and several do — the call
        // above succeeds, every frame encodes, and the far end shows a black
        // window forever because openh264 reads Constrained Baseline and
        // nothing else. That failure is invisible from this side: the encoder
        // reports no error at all, and the only symptom is on a different
        // machine. So the negotiated type is read back and the profile is
        // checked, and a driver that would not be pinned loses the job to the
        // software encoder, whose output is decodable by construction.
        let negotiated = transform.GetOutputCurrentType(0).map_err(init)?;
        match negotiated.GetUINT32(&MF_MT_MPEG2_PROFILE) {
            Ok(profile) if profile == eAVEncH264VProfile_Base.0 as u32 => {}
            Ok(profile) => {
                return Err(CodecError::Init {
                    codec: "media foundation",
                    reason: format!(
                        "this encoder ignored the request for Baseline and produces profile \
                         {profile}, which the other machine cannot decode"
                    ),
                })
            }
            // Absent, which is common and not evidence of anything: plenty of
            // transforms honour the requested profile without echoing it back
            // on the output type. Refusing here would cost the hardware
            // encoder — and the frame rate that comes with it — on the word of
            // a missing attribute rather than a wrong one.
            Err(_) => debug!("this encoder does not report its profile; taking Baseline as asked"),
        }

        let input = MFCreateMediaType().map_err(init)?;
        input
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(init)?;
        input
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
            .map_err(init)?;
        input
            .SetUINT64(
                &MF_MT_FRAME_SIZE,
                packed(resolution.width, resolution.height),
            )
            .map_err(init)?;
        input
            .SetUINT64(&MF_MT_FRAME_RATE, packed(fps, 1))
            .map_err(init)?;
        input
            .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, packed(1, 1))
            .map_err(init)?;
        input
            .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(init)?;
        transform.SetInputType(0, &input, 0).map_err(init)?;
    }

    Ok(())
}

fn init(error: windows::core::Error) -> CodecError {
    CodecError::Init {
        codec: "media foundation",
        reason: error.message(),
    }
}

/// Profile-specific knobs, applied where the driver supports them.
///
/// Every one of these is optional: `ICodecAPI` is a grab bag and vendors
/// implement different subsets of it, so a rejected property is a property this
/// encoder does not have rather than a reason to refuse the session. The
/// failures are logged at trace and otherwise ignored.
fn tune(api: &ICodecAPI, settings: &EncoderSettings) {
    use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};

    /// A `VARIANT` holding an unsigned long.
    fn number(value: u32) -> VARIANT {
        let mut variant = VARIANT::default();
        // SAFETY: writing the tag and the matching union field together, which
        // is the invariant a VARIANT has.
        unsafe {
            let inner = &mut variant.Anonymous.Anonymous;
            inner.vt = VT_UI4;
            inner.Anonymous.ulVal = value;
        }
        variant
    }

    /// A `VARIANT` holding a boolean, in the OLE sense where true is -1.
    fn flag(value: bool) -> VARIANT {
        let mut variant = VARIANT::default();
        // SAFETY: as above.
        unsafe {
            let inner = &mut variant.Anonymous.Anonymous;
            inner.vt = VT_BOOL;
            inner.Anonymous.boolVal =
                windows::Win32::Foundation::VARIANT_BOOL(if value { -1 } else { 0 });
        }
        variant
    }

    let set = |name: &GUID, value: VARIANT, what: &str| {
        // SAFETY: the property GUID and variant both outlive the call.
        if let Err(error) = unsafe { api.SetValue(name, &value) } {
            trace!(%error, what, "the encoder does not support this setting");
        }
    };

    // Constant bitrate for the two profiles that have a latency budget;
    // quality-targeted for the one that does not.
    let mode = if settings.profile == QualityProfile::Quality {
        eAVEncCommonRateControlMode_Quality
    } else {
        eAVEncCommonRateControlMode_CBR
    };
    set(
        &CODECAPI_AVEncCommonRateControlMode,
        number(mode.0 as u32),
        "rate control",
    );
    set(
        &CODECAPI_AVEncCommonMeanBitRate,
        number(settings.bitrate),
        "bitrate",
    );

    // The one that matters most. Low latency turns off the lookahead and the
    // frame reordering the encoder would otherwise use to compress better,
    // which is precisely the trade this product exists to make.
    set(&CODECAPI_AVEncCommonLowLatency, flag(true), "low latency");
    set(&CODECAPI_AVEncVideoEncodeQP, number(0), "fixed qp");

    // No B-frames: they cost a frame of delay each and they are what makes
    // output order differ from input order, which the timestamp queue relies
    // on not happening.
    set(
        &CODECAPI_AVEncMPVDefaultBPictureCount,
        number(0),
        "b-frames",
    );
    set(
        &CODECAPI_AVEncMPVGOPSize,
        number(settings.keyframe_frames().max(1)),
        "keyframe interval",
    );
}

impl VideoEncoder for HardwareEncoder {
    fn codec(&self) -> Codec {
        Codec::H264
    }

    fn resolution(&self) -> Resolution {
        self.resolution
    }

    fn encode(&mut self, frame: RawFrame<'_>) -> Result<Option<EncodedFrame>> {
        frame.check()?;
        enter_apartment();

        let cropped = Resolution::new(frame.resolution.width & !1, frame.resolution.height & !1);
        if cropped != self.resolution {
            return Err(CodecError::BadDimensions {
                resolution: frame.resolution,
                reason: "the display changed size; restart the stream",
            });
        }

        // The request went unanswered. Whatever this transform is doing with
        // `AVEncVideoForceKeyFrame`, it is not producing a keyframe, and the
        // far end cannot decode anything until one arrives — so it gets an
        // encoder that will.
        if self.keyframe_overdue() {
            debug!("the encoder ignored a keyframe request; opening a new one");
            self.rebuild()?;
        }

        self.begin()?;
        crate::convert::to_nv12(&frame, self.resolution, &mut self.nv12)?;

        // Read, not cleared: the flag is cleared only once a frame has actually
        // been handed over carrying it. Clearing it here would lose the request
        // whenever the transform happens to be busy this round, and a lost
        // keyframe request is a picture that never comes back.
        if self.keyframe_pending {
            if let Some(api) = &self.codec_api {
                // SAFETY: a well-known property GUID and a variant that lives
                // across the call.
                let mut variant = windows::Win32::System::Variant::VARIANT::default();
                unsafe {
                    let inner = &mut variant.Anonymous.Anonymous;
                    inner.vt = windows::Win32::System::Variant::VT_UI4;
                    inner.Anonymous.ulVal = 1;
                    let _ = api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &variant);
                }
            }
        }

        // Collect whatever the transform has said since last time, then answer
        // one request with this frame, then collect again so the frame it just
        // produced comes back in this call rather than the next one.
        self.pump()?;

        if self.credits > 0 {
            let sample = self.sample(self.counter * self.tick)?;
            // SAFETY: stream 0 is the only input stream, and the sample is
            // valid for the duration of the call.
            unsafe { self.transform.ProcessInput(0, &sample, 0) }
                .map_err(|error| CodecError::Encode(error.message()))?;
            if self.keyframe_pending {
                self.keyframe_pending = false;
                // Start the clock only now. A request made while the transform
                // was busy has not been put to it yet, and timing out on a
                // request nobody has heard would rebuild a working encoder.
                self.awaiting_keyframe = Some(self.counter);
            }
            self.credits -= 1;
            self.pending.push_back(frame.capture_micros);
            self.counter += 1;
            self.pump()?;
        } else {
            // The transform has not asked for a frame. Its input queue is full,
            // which on a hardware encoder means the GPU is the slow part —
            // dropping this frame is the right answer and is what the rate
            // controller would do anyway.
            self.seen.refused += 1;
            trace!("the encoder was not ready for a frame");
        }

        let Some((data, keyframe, capture_micros)) = self.ready.pop_front() else {
            return Ok(None);
        };

        Ok(Some(EncodedFrame {
            codec: Codec::H264,
            resolution: self.resolution,
            keyframe,
            capture_micros,
            data,
        }))
    }

    fn request_keyframe(&mut self) {
        self.keyframe_pending = true;
    }

    fn diagnostics(&self) -> Option<String> {
        let Seen {
            need_input,
            have_output,
            stream_changes,
            empty_outputs,
            refused,
            produced,
        } = self.seen;
        Some(format!(
            "given={} need_input={need_input} have_output={have_output} produced={produced} \
             stream_changes={stream_changes} empty_outputs={empty_outputs} refused={refused} \
             credits={} in_flight={}",
            self.counter,
            self.credits,
            self.pending.len()
        ))
    }
}

/// Join the multithreaded apartment on the calling thread, once per thread.
///
/// [`start_platform`] initialises COM on whichever thread first asks for an
/// encoder, but the encoder then runs on the host's streaming thread. That
/// thread only borrows the process's apartment implicitly, and only if one
/// exists — which it does not when the first caller was already in a
/// single-threaded apartment (the UI thread is). Saying so explicitly costs one
/// call per thread.
fn enter_apartment() {
    use std::cell::Cell;
    thread_local! {
        static ENTERED: Cell<bool> = const { Cell::new(false) };
    }
    ENTERED.with(|entered| {
        if !entered.get() {
            entered.set(true);
            // SAFETY: the documented per-thread initialiser; a thread already
            // in an apartment gets an error back, which changes nothing.
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
        }
    });
}

/// Take an interface out of a `MFT_OUTPUT_DATA_BUFFER` slot, leaving it empty.
///
/// The struct holds its interface pointers in `ManuallyDrop` because Media
/// Foundation may or may not fill them in, so the caller owns whatever is there
/// afterwards and nothing releases it automatically. Taking the value and
/// putting `None` back means it is released exactly once — leaving it would
/// leak a COM object per frame, and dropping the struct without taking it would
/// leak it too.
fn claim<T>(slot: &mut std::mem::ManuallyDrop<Option<T>>) -> Option<T> {
    // SAFETY: the slot is overwritten with `None` immediately, so the value is
    // moved out exactly once and nothing reads the vacated bytes.
    unsafe {
        let taken = std::mem::ManuallyDrop::take(slot);
        *slot = std::mem::ManuallyDrop::new(None);
        taken
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_size_is_packed_high_half_first() {
        assert_eq!(packed(1920, 1080), (1920u64 << 32) | 1080);
    }

    #[test]
    fn a_parameter_set_or_an_idr_is_a_keyframe() {
        // Four-byte start code before an SPS, which is how most encoders open
        // an access unit.
        assert!(starts_a_keyframe(&[0, 0, 0, 1, 0x67, 0x42, 0x00]));
        // Three-byte start code before an IDR slice.
        assert!(starts_a_keyframe(&[0, 0, 1, 0x65, 0x88, 0x84]));
        // Mixed within one access unit: SPS, then PPS, then the IDR. Missing
        // this shape is the whole reason both lengths are accepted.
        assert!(starts_a_keyframe(&[
            0, 0, 0, 1, 0x67, 0x42, 0, 0, 1, 0x68, 0xce, 0, 0, 1, 0x65, 0x88,
        ]));
    }

    #[test]
    fn a_predicted_frame_is_not_mistaken_for_one() {
        // Type 1, a non-IDR slice — the ordinary frame. Reporting this as a
        // keyframe would be worse than missing one: the client would start
        // decoding from a frame that refers to pictures it has never seen.
        assert!(!starts_a_keyframe(&[0, 0, 0, 1, 0x41, 0x9a, 0x00]));
        // Type 6, supplemental enhancement information. Carries no picture.
        assert!(!starts_a_keyframe(&[0, 0, 1, 0x06, 0x05, 0x10]));
        assert!(!starts_a_keyframe(&[]));
        assert!(!starts_a_keyframe(&[0, 0, 1]));
    }

    #[test]
    fn a_byte_with_its_top_bit_set_is_not_a_nal_header() {
        // `forbidden_zero_bit` is one, so this is not a NAL header and its low
        // five bits mean nothing. They spell 5 here on purpose: taken as a
        // type it would read as an IDR and let the client start from a frame
        // that is not one.
        assert!(!starts_a_keyframe(&[0, 0, 1, 0x85, 0x00]));
    }

    #[test]
    fn asking_whether_hardware_exists_never_panics() {
        // Called during capability probing on every machine, including ones
        // with no GPU, no Media Foundation, and no display at all.
        let _ = hardware_h264_available();
    }
}
