//! A manifest as frames: independent runs of bytes, one per part and one for
//! the index.
//!
//! # How a write works
//!
//! 1. Choose a [`Codec`]. [`Plain`] does no compression, and the `zstd`
//!    feature has the zstd one.
//! 2. Call [`write_frames`] with the root, a [`WalkOptions`], the codec, a
//!    thread count and a sink. It walks the root and encodes each part on a
//!    worker thread as soon as the walk seals it.
//! 3. The sink receives one [`Frame`] per part, in walk order, and then the
//!    [`Frame`] of the index. Put each one wherever you keep it.
//!
//! Every frame is complete on its own: [`decode_line`] needs nothing but the
//! frame's bytes and the codec. A frame decodes to one line of text: the JSON
//! of a part or of the index, followed by a newline. The frames of a manifest
//! decoded in order are therefore a JSON Lines document.
//!
//! [`file::write_file`](crate::file::write_file) is a sink that writes the
//! frames one after another into a single file.
//!
//! # Memory
//!
//! Each worker thread keeps one [`Encoder`], one JSON buffer and one output
//! buffer, and reuses them for every part. The walk waits when every thread is
//! busy and the queue of sealed parts, one slot per thread, is full.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;

use error_stack::{Report, ResultExt as _};

use crate::manifest::{Index, PartEntry, TreePart};
use crate::walk::{WalkError, WalkOptions, walk_parts};

/// The number of worker threads the `tarseer` command uses.
pub const DEFAULT_THREADS: usize = 2;

/// The frames could not be written.
///
/// If the walk failed, its [`WalkError`] is inside the report:
/// `report.contains::<WalkError>()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteError;

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("could not write the manifest")
    }
}

impl std::error::Error for WriteError {}

/// The bytes are not a frame or a manifest this build can read. The report
/// says what does not hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadError;

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not a readable tarseer manifest")
    }
}

impl std::error::Error for ReadError {}

/// Turns the text of a part or an index into a frame and back.
///
/// Implement this to try another compression method. [`write_frames`] and
/// [`decode_line`] use nothing of a codec but these two methods.
pub trait Codec: Send + Sync {
    /// Creates an encoder. Each worker thread creates one and reuses it for
    /// every frame it writes.
    ///
    /// # Errors
    /// [`WriteError`] if the encoder cannot be set up, for example because the
    /// codec's parameters are out of range.
    fn encoder(&self) -> Result<Box<dyn Encoder>, Report<WriteError>>;

    /// Decodes one frame.
    ///
    /// # Errors
    /// [`ReadError`] if `frame` is not exactly one frame of this codec, or is
    /// damaged.
    fn decode(&self, frame: &[u8]) -> Result<Vec<u8>, Report<ReadError>>;
}

/// The state one thread keeps between frames.
pub trait Encoder: Send {
    /// Clears `frame` and writes `raw` into it as one frame.
    ///
    /// # Errors
    /// [`WriteError`] if `raw` cannot be encoded.
    fn encode(&mut self, raw: &[u8], frame: &mut Vec<u8>) -> Result<(), Report<WriteError>>;
}

/// The codec that changes nothing: a frame is the text itself.
#[derive(Debug, Clone, Copy, Default)]
pub struct Plain;

impl Codec for Plain {
    fn encoder(&self) -> Result<Box<dyn Encoder>, Report<WriteError>> {
        Ok(Box::new(Self))
    }

    fn decode(&self, frame: &[u8]) -> Result<Vec<u8>, Report<ReadError>> {
        Ok(frame.to_vec())
    }
}

impl Encoder for Plain {
    fn encode(&mut self, raw: &[u8], frame: &mut Vec<u8>) -> Result<(), Report<WriteError>> {
        frame.clear();
        frame.extend_from_slice(raw);
        Ok(())
    }
}

/// What a [`Frame`] holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// The part with this number, counting from 0 in walk order.
    Part(usize),
    /// The index. It is always the last frame.
    Index,
}

/// One encoded part, or the encoded index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Which part this is, or that it is the index.
    pub kind: FrameKind,
    /// The encoded bytes.
    pub bytes: Vec<u8>,
    /// The length in bytes of the text that `bytes` decodes to.
    pub raw_len: u64,
}

// An encoded part on its way to the sink, with its index entry.
struct Encoded {
    sequence: usize,
    bytes: Vec<u8>,
    raw_len: u64,
    entry: PartEntry,
}

/// Walks `root` and hands `sink` one [`Frame`] per part, in walk order, then
/// the frame of the index. Returns the index.
///
/// The walk runs on the calling thread and `sink` on a thread of its own.
/// Each part is encoded on one of `threads` worker threads; `0` is treated as
/// 1. The frames do not depend on the number of threads.
///
/// # Errors
/// [`WriteError`] if the walk failed (its [`WalkError`] is inside the
/// report), if a part or the index could not be written as JSON or encoded,
/// or if `sink` returned an error.
///
/// # Panics
/// If a worker thread or `sink` panics.
pub fn write_frames(
    root: &Path,
    walk_options: &WalkOptions<'_>,
    codec: &dyn Codec,
    threads: usize,
    sink: &mut (dyn FnMut(Frame) -> Result<(), Report<WriteError>> + Send),
) -> Result<Index, Report<WriteError>> {
    let threads = threads.max(1);
    let part_sink = &mut *sink;

    let (in_order, walked) = thread::scope(|scope| {
        // Bounded, so a walk that outruns encoding waits instead of queueing
        // parts without limit.
        let (job_sender, job_receiver) = mpsc::sync_channel::<(usize, TreePart)>(threads);
        let job_receiver = Arc::new(Mutex::new(job_receiver));
        let (done_sender, done_receiver) = mpsc::channel();
        for _ in 0..threads {
            let job_receiver = Arc::clone(&job_receiver);
            let done_sender = done_sender.clone();
            scope.spawn(move || {
                let mut worker = match Worker::new(codec) {
                    Ok(worker) => worker,
                    Err(report) => {
                        let _ = done_sender.send(Err(report));
                        return;
                    }
                };
                loop {
                    let job = job_receiver
                        .lock()
                        .expect("no worker panics holding the job queue")
                        .recv();
                    let Ok((sequence, part)) = job else { break };
                    if done_sender.send(worker.encode(sequence, &part)).is_err() {
                        break;
                    }
                }
            });
        }
        // Only the workers hold these now: when they have all stopped, the
        // walk's next send fails instead of waiting forever.
        drop(job_receiver);
        drop(done_sender);
        let orderer = scope.spawn(move || hand_out_in_order(done_receiver, part_sink));

        let mut sequence = 0;
        let walked = walk_parts(root, walk_options, &mut |part| {
            if job_sender.send((sequence, part)).is_err() {
                // The sink's thread stopped; its own error is the one returned.
                return Err(Report::new(WalkError));
            }
            sequence += 1;
            Ok(())
        });
        drop(job_sender);
        let in_order = orderer.join().expect("the sink does not panic");
        (in_order, walked)
    });
    let parts = in_order?;
    let skips = walked.change_context(WriteError)?;

    let index = Index { parts, skips };
    let mut line = index.to_json().change_context(WriteError)?.into_bytes();
    line.push(b'\n');
    let mut bytes = Vec::new();
    codec.encoder()?.encode(&line, &mut bytes)?;
    sink(Frame {
        kind: FrameKind::Index,
        bytes,
        raw_len: line.len() as u64,
    })?;
    Ok(index)
}

// One worker's reusable state. Only the frame, a fraction of the JSON's size
// under a compressing codec, is allocated per part.
struct Worker {
    encoder: Box<dyn Encoder>,
    line: Vec<u8>,
    frame: Vec<u8>,
}

impl Worker {
    fn new(codec: &dyn Codec) -> Result<Self, Report<WriteError>> {
        Ok(Self {
            encoder: codec.encoder()?,
            line: Vec::new(),
            frame: Vec::new(),
        })
    }

    fn encode(&mut self, sequence: usize, part: &TreePart) -> Result<Encoded, Report<WriteError>> {
        // An eighth more than the estimate, so that the JSON fits.
        room(&mut self.line, part.estimate() + part.estimate() / 8);
        part.write_json(&mut self.line).change_context(WriteError)?;
        self.line.push(b'\n');
        self.encoder.encode(&self.line, &mut self.frame)?;
        Ok(Encoded {
            sequence,
            bytes: self.frame.clone(),
            raw_len: self.line.len() as u64,
            entry: PartEntry::of(part),
        })
    }
}

/// Empties `buffer` and makes room in it for `bytes`. A buffer that is too
/// small is replaced, not grown: growing would copy what it held, which is of
/// no more use, and an allocator may copy it into memory it has not touched.
pub(crate) fn room(buffer: &mut Vec<u8>, bytes: usize) {
    if buffer.capacity() < bytes {
        *buffer = Vec::with_capacity(bytes);
    } else {
        buffer.clear();
    }
}

// Hand encoded parts to the sink as they arrive, holding back any that come
// early. Returns the index entries.
fn hand_out_in_order(
    done: Receiver<Result<Encoded, Report<WriteError>>>,
    sink: &mut (dyn FnMut(Frame) -> Result<(), Report<WriteError>> + Send),
) -> Result<Vec<PartEntry>, Report<WriteError>> {
    let mut early = BTreeMap::new();
    let mut entries = Vec::new();
    for encoded in done {
        let encoded = encoded?;
        early.insert(encoded.sequence, encoded);
        while let Some(encoded) = early.remove(&entries.len()) {
            sink(Frame {
                kind: FrameKind::Part(entries.len()),
                bytes: encoded.bytes,
                raw_len: encoded.raw_len,
            })?;
            entries.push(encoded.entry);
        }
    }
    if let Some(&sequence) = early.keys().next() {
        return Err(WriteError).attach_with(|| {
            format!(
                "part {} never arrived, but part {sequence} did",
                entries.len()
            )
        });
    }
    Ok(entries)
}

/// Decodes one frame and returns its line of JSON, without the newline.
///
/// # Errors
/// [`ReadError`] if `codec` cannot decode `frame`, or if what it decodes to
/// does not end in a newline.
pub fn decode_line(codec: &dyn Codec, frame: &[u8]) -> Result<Vec<u8>, Report<ReadError>> {
    let mut line = codec.decode(frame)?;
    if line.pop() != Some(b'\n') {
        return Err(ReadError).attach_with(|| "the frame does not end in a newline".to_owned());
    }
    Ok(line)
}

/// Decodes the frame of an index.
///
/// # Errors
/// [`ReadError`] as [`decode_line`], or if the line is not the JSON of an
/// index.
pub fn decode_index(codec: &dyn Codec, frame: &[u8]) -> Result<Index, Report<ReadError>> {
    let line = decode_line(codec, frame)?;
    Index::from_json(&line).change_context(ReadError)
}
