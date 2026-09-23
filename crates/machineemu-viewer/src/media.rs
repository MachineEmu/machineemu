//! GStreamer decode and playback.
//!
//! Video runs `appsrc ! h264parse ! <decoder> ! <postproc> ! capsfilter !
//! appsink`. Hardware decoders are preferred. With VA-API, `vapostproc` does
//! colour conversion and scaling on the GPU, and the only CPU copy is one BGRx
//! readback at the window's viewport size. The capsfilter follows the window,
//! so the pipeline scales for the viewer.

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use gstreamer_video::prelude::*;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::{debug, info, trace, warn};

use crate::protocol::AudioConfig;

/// One decoded frame as tightly packed `0x00RRGGBB` pixels.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u32>,
}

/// Hardware decoders in order of preference, each with the element that
/// converts to BGRx and scales. Only VA-API has a GPU post-processor that
/// outputs system memory directly; the others use `videoconvertscale`.
const HARDWARE: &[(&str, &str)] = &[
    ("vah264dec", "vapostproc"),
    ("nvh264dec", "videoconvertscale"),
    ("v4l2slh264dec", "videoconvertscale"),
    ("v4l2h264dec", "videoconvertscale"),
    ("d3d12h264dec", "videoconvertscale"),
    ("d3d11h264dec", "videoconvertscale"),
    ("vtdec_hw", "videoconvertscale"),
];
const SOFTWARE: &[&str] = &["avdec_h264", "openh264dec"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecoderChoice {
    /// The first available hardware decoder. Software is used when there is
    /// none or when the hardware decoder fails at run time.
    Auto,
    /// Hardware only. The viewer fails instead of falling back.
    Hardware,
    Software,
    /// A specific GStreamer element name.
    Element(String),
}

impl FromStr for DecoderChoice {
    type Err = std::convert::Infallible;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(match value {
            "auto" => Self::Auto,
            "hardware" => Self::Hardware,
            "software" => Self::Software,
            other => Self::Element(other.to_owned()),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecoderPlan {
    pub decoder: String,
    pub postproc: &'static str,
    pub hardware: bool,
}

fn available(name: &str) -> bool {
    gst::ElementFactory::find(name).is_some()
}

fn is_hardware(name: &str) -> bool {
    gst::ElementFactory::find(name).is_some_and(|factory| factory.klass().contains("Hardware"))
}

fn plan_for(name: &str) -> DecoderPlan {
    let postproc = HARDWARE
        .iter()
        .find(|(decoder, _)| *decoder == name)
        .map(|(_, postproc)| *postproc)
        .unwrap_or(if name.starts_with("va") && available("vapostproc") {
            "vapostproc"
        } else {
            "videoconvertscale"
        });
    DecoderPlan {
        decoder: name.to_owned(),
        postproc,
        hardware: is_hardware(name),
    }
}

/// Resolve a decoder choice against the elements registered on this host.
/// VA-API registers `vah264dec` only when a device can decode H.264, so the
/// element existing means hardware decode is available.
pub fn select(choice: &DecoderChoice) -> Result<DecoderPlan> {
    let hardware = || {
        HARDWARE
            .iter()
            .find(|(decoder, postproc)| available(decoder) && available(postproc))
            .map(|(decoder, _)| plan_for(decoder))
    };
    let software = || {
        SOFTWARE
            .iter()
            .find(|decoder| available(decoder))
            .map(|decoder| plan_for(decoder))
    };
    match choice {
        DecoderChoice::Auto => hardware().or_else(software).ok_or_else(|| {
            anyhow!("no H.264 decoder: install gst-plugins-bad (VA-API) or gst-libav")
        }),
        DecoderChoice::Hardware => hardware().ok_or_else(|| {
            anyhow!(
                "no hardware H.264 decoder; tried {}",
                HARDWARE
                    .iter()
                    .map(|(d, _)| *d)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }),
        DecoderChoice::Software => software()
            .ok_or_else(|| anyhow!("no software H.264 decoder: install gst-libav or openh264")),
        DecoderChoice::Element(name) if available(name) => Ok(plan_for(name)),
        DecoderChoice::Element(name) => bail!("GStreamer element {name} is not installed"),
    }
}

/// The software plan used after a hardware decoder fails in `Auto` mode.
pub fn software_fallback() -> Option<DecoderPlan> {
    select(&DecoderChoice::Software).ok()
}

/// An error from a video pipeline, tagged with the pipeline generation so a
/// late message from a replaced pipeline can be ignored.
pub struct VideoError {
    pub generation: u64,
    pub message: String,
}

type FrameSink = Arc<dyn Fn(Frame) + Send + Sync>;

/// Running totals for the periodic stream statistics.
#[derive(Default)]
pub struct VideoCounters {
    /// Access units given to the decoder.
    pub pushed: AtomicU64,
    pub pushed_bytes: AtomicU64,
    /// Frames that came out of the decoder and post-processor.
    pub decoded: AtomicU64,
    /// Decoded frames replaced before the window drew them.
    pub superseded: AtomicU64,
}

/// Write the pipeline graph when `GST_DEBUG_DUMP_DOT_DIR` is set; GStreamer
/// makes this a no-op otherwise. Render with `dot -Tsvg`.
fn dump_graph(pipeline: &gst::Pipeline, name: &str) {
    pipeline.debug_to_dot_file_with_ts(gst::DebugGraphDetails::all(), name);
}

struct VideoPipeline {
    pipeline: gst::Pipeline,
    src: gst_app::AppSrc,
    size: gst::Element,
    plan: DecoderPlan,
    generation: u64,
    base_pts: Option<u64>,
}

impl Drop for VideoPipeline {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// The video decoder, shared by the network task (which feeds it) and the
/// window (which sets the output size).
pub struct Video {
    current: Mutex<VideoPipeline>,
    output: Mutex<Option<(u32, u32)>>,
    on_frame: FrameSink,
    errors: mpsc::UnboundedSender<VideoError>,
    counters: Arc<VideoCounters>,
}

fn output_caps(size: Option<(u32, u32)>) -> gst::Caps {
    let builder = gst::Caps::builder("video/x-raw")
        .field("format", "BGRx")
        .field("pixel-aspect-ratio", gst::Fraction::new(1, 1));
    match size {
        Some((width, height)) => builder
            .field("width", width as i32)
            .field("height", height as i32)
            .build(),
        None => builder.build(),
    }
}

fn frame_from_sample(sample: &gst::Sample) -> Option<Frame> {
    let info = gst_video::VideoInfo::from_caps(sample.caps()?).ok()?;
    if info.format() != gst_video::VideoFormat::Bgrx {
        return None;
    }
    let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(sample.buffer()?, &info).ok()?;
    let (width, height) = (info.width(), info.height());
    let stride = frame.plane_stride()[0] as usize;
    let data = frame.plane_data(0).ok()?;
    let row_bytes = width as usize * 4;
    let mut pixels = Vec::with_capacity(width as usize * height as usize);
    for row in 0..height as usize {
        let line = data.get(row * stride..row * stride + row_bytes)?;
        pixels.extend(
            line.as_chunks::<4>()
                .0
                .iter()
                .map(|[b, g, r, _]| u32::from_le_bytes([*b, *g, *r, 0])),
        );
    }
    Some(Frame {
        width,
        height,
        pixels,
    })
}

fn build_video(
    plan: &DecoderPlan,
    generation: u64,
    output: Option<(u32, u32)>,
    on_frame: FrameSink,
    errors: mpsc::UnboundedSender<VideoError>,
    counters: Arc<VideoCounters>,
) -> Result<VideoPipeline> {
    let description = format!(
        "appsrc name=src is-live=true format=time ! h264parse ! {} ! {} ! capsfilter name=size ! appsink name=sink sync=false max-buffers=1 drop=true",
        plan.decoder, plan.postproc
    );
    let pipeline = gst::parse::launch(&description)
        .context("building the video pipeline")?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("video pipeline is not a gst::Pipeline"))?;
    let element = |name: &str| {
        pipeline
            .by_name(name)
            .ok_or_else(|| anyhow!("video pipeline has no {name} element"))
    };
    let src = element("src")?
        .downcast::<gst_app::AppSrc>()
        .map_err(|_| anyhow!("src is not an appsrc"))?;
    src.set_caps(Some(
        &gst::Caps::builder("video/x-h264")
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .build(),
    ));
    let size = element("size")?;
    size.set_property("caps", output_caps(output));
    let sink = element("sink")?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| anyhow!("sink is not an appsink"))?;
    let last_caps = Mutex::new(None::<gst::Caps>);
    let weak = pipeline.downgrade();
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                if let Some(caps) = sample.caps() {
                    let mut last = last_caps.lock().expect("caps lock poisoned");
                    if last.as_ref() != Some(&caps.to_owned()) {
                        debug!(generation, %caps, "video output negotiated");
                        if let Some(pipeline) = weak.upgrade() {
                            dump_graph(&pipeline, &format!("video-{generation}-negotiated"));
                        }
                        *last = Some(caps.to_owned());
                    }
                }
                match frame_from_sample(&sample) {
                    Some(frame) => {
                        counters.decoded.fetch_add(1, Ordering::Relaxed);
                        trace!(width = frame.width, height = frame.height, "frame decoded");
                        on_frame(frame);
                    }
                    None => warn!(generation, "decoded sample is not readable BGRx"),
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    let bus = pipeline.bus().context("video pipeline has no bus")?;
    let weak = pipeline.downgrade();
    bus.set_sync_handler(move |_, message| {
        match message.view() {
            gst::MessageView::Error(error) => {
                let source = error
                    .src()
                    .map(|source| source.name().to_string())
                    .unwrap_or_default();
                warn!(
                    generation,
                    source,
                    error = %error.error(),
                    debug = ?error.debug(),
                    "video pipeline error"
                );
                if let Some(pipeline) = weak.upgrade() {
                    dump_graph(&pipeline, &format!("video-{generation}-error"));
                }
                let _ = errors.send(VideoError {
                    generation,
                    message: format!("{source}: {}", error.error()),
                });
            }
            gst::MessageView::Warning(warning) => {
                let source = warning
                    .src()
                    .map(|source| source.name().to_string())
                    .unwrap_or_default();
                warn!(generation, source, warning = %warning.error(), debug = ?warning.debug(), "video pipeline warning");
            }
            gst::MessageView::StateChanged(change)
                if change.src().is_some_and(|source| source.type_().is_a(gst::Pipeline::static_type())) =>
            {
                debug!(generation, from = ?change.old(), to = ?change.current(), "video pipeline state");
            }
            _ => {}
        }
        gst::BusSyncReply::Drop
    });
    info!(generation, pipeline = %description, "starting video pipeline");
    pipeline
        .set_state(gst::State::Playing)
        .context("starting the video pipeline")?;
    Ok(VideoPipeline {
        pipeline,
        src,
        size,
        plan: plan.clone(),
        generation,
        base_pts: None,
    })
}

impl Video {
    pub fn new(
        plan: &DecoderPlan,
        counters: Arc<VideoCounters>,
        on_frame: impl Fn(Frame) + Send + Sync + 'static,
        errors: mpsc::UnboundedSender<VideoError>,
    ) -> Result<Self> {
        let on_frame: FrameSink = Arc::new(on_frame);
        let current = build_video(
            plan,
            0,
            None,
            on_frame.clone(),
            errors.clone(),
            counters.clone(),
        )?;
        Ok(Self {
            current: Mutex::new(current),
            output: Mutex::new(None),
            on_frame,
            errors,
            counters,
        })
    }

    pub fn counters(&self) -> &VideoCounters {
        &self.counters
    }

    pub fn plan(&self) -> (DecoderPlan, u64) {
        let current = self.current.lock().expect("video lock poisoned");
        (current.plan.clone(), current.generation)
    }

    /// Replace the pipeline with one using `plan`. The caller should request a
    /// keyframe, because the new decoder cannot use earlier deltas.
    pub fn rebuild(&self, plan: &DecoderPlan) -> Result<()> {
        let output = *self.output.lock().expect("output lock poisoned");
        let mut current = self.current.lock().expect("video lock poisoned");
        let next = build_video(
            plan,
            current.generation + 1,
            output,
            self.on_frame.clone(),
            self.errors.clone(),
            self.counters.clone(),
        )?;
        info!(
            from = current.plan.decoder,
            to = plan.decoder,
            generation = next.generation,
            "video decoder replaced"
        );
        *current = next;
        Ok(())
    }

    /// Queue one Annex-B access unit.
    pub fn push(&self, payload: Bytes, pts_us: u64) -> Result<()> {
        self.counters.pushed.fetch_add(1, Ordering::Relaxed);
        self.counters
            .pushed_bytes
            .fetch_add(payload.len() as u64, Ordering::Relaxed);
        let mut current = self.current.lock().expect("video lock poisoned");
        let base = *current.base_pts.get_or_insert(pts_us);
        let mut buffer = gst::Buffer::from_slice(payload);
        buffer
            .get_mut()
            .expect("new buffer is writable")
            .set_pts(gst::ClockTime::from_useconds(pts_us.saturating_sub(base)));
        current
            .src
            .push_buffer(buffer)
            .map_err(|error| anyhow!("video decoder rejected data: {error:?}"))?;
        Ok(())
    }

    /// Ask the post-processor to scale to this size, or `None` for the
    /// guest's native size. The change applies from the next decoded frame.
    pub fn set_output_size(&self, size: Option<(u32, u32)>) {
        let size = size.map(|(w, h)| (w.max(16), h.max(16)));
        let mut output = self.output.lock().expect("output lock poisoned");
        if *output == size {
            return;
        }
        debug!(?size, "video output size requested");
        *output = size;
        let current = self.current.lock().expect("video lock poisoned");
        current.size.set_property("caps", output_caps(size));
    }
}

struct AudioPipeline {
    pipeline: gst::Pipeline,
    src: gst_app::AppSrc,
    volume: gst::Element,
    caps: String,
}

impl Drop for AudioPipeline {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Guest audio playback. Built on the first enabled configuration and
/// rebuilt when the sample format changes.
#[derive(Default)]
pub struct Audio {
    current: Option<AudioPipeline>,
}

impl Audio {
    pub fn configure(&mut self, config: &AudioConfig) -> Result<()> {
        debug!(?config, "guest audio configuration");
        if !config.enabled {
            if self.current.is_some() {
                info!("guest audio stopped");
            }
            self.current = None;
            return Ok(());
        }
        let caps = config
            .caps()
            .ok_or_else(|| anyhow!("unsupported guest audio format: {config:?}"))?;
        if self
            .current
            .as_ref()
            .is_none_or(|current| current.caps != caps)
        {
            self.current = None;
            info!(caps, "starting audio pipeline");
            self.current = Some(build_audio(&caps)?);
        }
        if let Some(current) = &self.current {
            current.volume.set_property("volume", config.gain());
        }
        Ok(())
    }

    pub fn push(&mut self, data: Bytes) -> Result<()> {
        if let Some(current) = &self.current {
            current
                .src
                .push_buffer({
                    trace!(bytes = data.len(), "audio data");
                    gst::Buffer::from_slice(data)
                })
                .map_err(|error| anyhow!("audio sink rejected data: {error:?}"))?;
        }
        Ok(())
    }
}

fn build_audio(caps: &str) -> Result<AudioPipeline> {
    // The guest produces audio in real time, so the sink plays what arrives
    // instead of scheduling by timestamp; a leaky queue bounds latency.
    let pipeline = gst::parse::launch(
        "appsrc name=src is-live=true format=time do-timestamp=true ! queue max-size-time=200000000 max-size-buffers=0 max-size-bytes=0 leaky=downstream ! audioconvert ! audioresample ! volume name=volume ! autoaudiosink sync=false",
    )
    .context("building the audio pipeline")?
    .downcast::<gst::Pipeline>()
    .map_err(|_| anyhow!("audio pipeline is not a gst::Pipeline"))?;
    let src = pipeline
        .by_name("src")
        .and_then(|element| element.downcast::<gst_app::AppSrc>().ok())
        .context("audio pipeline has no appsrc")?;
    src.set_caps(Some(
        &gst::Caps::from_str(caps).with_context(|| format!("invalid audio caps {caps}"))?,
    ));
    let volume = pipeline
        .by_name("volume")
        .context("audio pipeline has no volume element")?;
    let weak = pipeline.downgrade();
    pipeline
        .bus()
        .context("audio pipeline has no bus")?
        .set_sync_handler(move |_, message| {
            match message.view() {
                gst::MessageView::Error(error) => {
                    warn!(error = %error.error(), debug = ?error.debug(), "audio pipeline error");
                    if let Some(pipeline) = weak.upgrade() {
                        dump_graph(&pipeline, "audio-error");
                    }
                }
                gst::MessageView::Warning(warning) => {
                    warn!(warning = %warning.error(), debug = ?warning.debug(), "audio pipeline warning");
                }
                _ => {}
            }
            gst::BusSyncReply::Drop
        });
    pipeline
        .set_state(gst::State::Playing)
        .context("starting the audio pipeline")?;
    Ok(AudioPipeline {
        pipeline,
        src,
        volume,
        caps: caps.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc as std_mpsc;
    use std::time::Duration;

    /// Encode test frames to Annex-B access units, as display-stream sends.
    fn encoded_units(count: u32) -> Option<Vec<Bytes>> {
        let encoder = ["vah264enc", "openh264enc", "x264enc"]
            .into_iter()
            .find(|name| available(name))?;
        let pipeline = gst::parse::launch(&format!(
            "videotestsrc num-buffers={count} ! video/x-raw,format=NV12,width=640,height=360,framerate=30/1 ! {encoder} ! h264parse ! video/x-h264,stream-format=byte-stream,alignment=au ! appsink name=sink sync=false"
        ))
        .ok()?
        .downcast::<gst::Pipeline>()
        .ok()?;
        let sink = pipeline
            .by_name("sink")?
            .downcast::<gst_app::AppSink>()
            .ok()?;
        pipeline.set_state(gst::State::Playing).ok()?;
        let mut units = Vec::new();
        while let Ok(sample) = sink.pull_sample() {
            let map = sample.buffer()?.map_readable().ok()?;
            units.push(Bytes::copy_from_slice(&map));
        }
        pipeline.set_state(gst::State::Null).ok()?;
        Some(units)
    }

    fn decode_with(
        choice: DecoderChoice,
        output: Option<(u32, u32)>,
    ) -> Option<(DecoderPlan, (u32, u32))> {
        gst::init().unwrap();
        let plan = select(&choice).ok()?;
        let units = encoded_units(30)?;
        let (frames, received) = std_mpsc::channel();
        let (errors, mut error_events) = mpsc::unbounded_channel();
        let frames = Mutex::new(frames);
        let video = Video::new(
            &plan,
            Arc::default(),
            move |frame| {
                let _ = frames.lock().unwrap().send((frame.width, frame.height));
            },
            errors,
        )
        .unwrap();
        video.set_output_size(output);
        for (index, unit) in units.into_iter().enumerate() {
            video.push(unit, index as u64 * 33_333).unwrap();
        }
        let size = received.recv_timeout(Duration::from_secs(10));
        if let Ok(error) = error_events.try_recv() {
            panic!("{} failed: {}", plan.decoder, error.message);
        }
        Some((plan, size.expect("no decoded frame")))
    }

    #[test]
    fn hardware_decoder_scales_on_the_gpu_when_present() {
        let Some((plan, size)) = decode_with(DecoderChoice::Hardware, Some((320, 180))) else {
            eprintln!("skipped: no hardware H.264 decoder or encoder");
            return;
        };
        assert!(plan.hardware, "{plan:?}");
        assert_eq!(size, (320, 180));
    }

    #[test]
    fn software_decoder_decodes_at_native_size() {
        let Some((plan, size)) = decode_with(DecoderChoice::Software, None) else {
            eprintln!("skipped: no software H.264 decoder or encoder");
            return;
        };
        assert!(!plan.hardware, "{plan:?}");
        assert_eq!(size, (640, 360));
    }
}
