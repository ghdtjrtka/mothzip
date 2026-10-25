//! Compression stream backends are one input case each; container handling is shared.
//!
//! A stream codec turns a byte stream into a compressed stream and back. The container
//! (TAR or a single member) is decided elsewhere, so every codec is reachable through the
//! same reader/writer producers in `format`, `pack`, `extract`, `inspect`, and `catalog`.
//!
//! Contract for every backend module (`zstd`, `xz`, `lz4`) and for the built-in GZIP/Brotli
//! branches here:
//!
//! * `encoder` writes exactly one stream/frame/member into the sink it is given, and `finish`
//!   writes the codec trailer into that same sink. The sink is generic over `Write`, because an
//!   encoder only ever writes forward: a file, a pipe (`pack --to-stdout`), or a test buffer are
//!   the same producer with a different destination.
//! * `decoder` yields every decompressed byte of a valid stream. Truncated or corrupt input is
//!   an `ErrorKind::InvalidData` error, never a short read that looks like success.
//! * Concatenated members/frames/streams are decoded when the format defines concatenation
//!   (gzip multi-member, zstd multi-frame, xz multi-stream). Bytes that cannot be decoded after
//!   the last member are an `ErrorKind::InvalidData` error, never silently ignored.
//! * Memory stays bounded: a decoder never buffers the whole stream to answer `read`.
use std::fs::File;
use std::io::{self, Read, Write};
use std::ops::RangeInclusive;

/// An encoder that owns its output sink and can be finalized into it.
pub trait FinishEncoder: Write {
    /// Write the codec trailer into the sink and return.
    ///
    /// The sink is not handed back because it is not always a file: a `--to-stdout` archive is
    /// written into a pipe that the caller must not also write to. A caller that needs the
    /// finished file reopens it by path, which is the one extra open this contract costs.
    fn finish(self: Box<Self>) -> io::Result<()>;
}

#[path = "codec/lz4.rs"]
pub mod lz4;
#[path = "codec/xz.rs"]
pub mod xz;
#[path = "codec/zstd.rs"]
pub mod zstd;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamCodec {
    Gzip,
    Zstd,
    Xz,
    Lz4,
    Brotli,
}

impl StreamCodec {
    /// File-name suffix of a single stream file (`data.txt.gz`, `data.txt.zst`, ...).
    pub fn suffix(self) -> &'static str {
        match self {
            Self::Gzip => "gz",
            Self::Zstd => "zst",
            Self::Xz => "xz",
            Self::Lz4 => "lz4",
            Self::Brotli => "br",
        }
    }

    /// Display name used in CLI output and error messages.
    pub fn name(self) -> &'static str {
        match self {
            Self::Gzip => "GZIP",
            Self::Zstd => "Zstd",
            Self::Xz => "XZ",
            Self::Lz4 => "LZ4",
            Self::Brotli => "Brotli",
        }
    }

    /// Accepted `--level` values, or `None` when the codec has no compression level.
    pub fn level_range(self) -> Option<RangeInclusive<u32>> {
        match self {
            Self::Gzip => Some(0..=9),
            Self::Zstd => Some(1..=22),
            Self::Xz => Some(0..=9),
            Self::Lz4 => None,
            Self::Brotli => Some(0..=11),
        }
    }
}

/// Single producer of the accepted level range: callers pass the codec and the requested level,
/// and an out-of-range level is rejected with the valid range in the message.
pub fn validated_level(codec: StreamCodec, level: Option<u32>) -> io::Result<Option<u32>> {
    let Some(level) = level else {
        return Ok(None);
    };
    match codec.level_range() {
        Some(range) => {
            if range.contains(&level) {
                Ok(Some(level))
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "{} compression level must be {}..={}",
                        codec.name(),
                        range.start(),
                        range.end()
                    ),
                ))
            }
        }
        None => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} streams carry no compression level", codec.name()),
        )),
    }
}

/// Open a decompressing reader over `file` for the selected codec.
pub fn decoder(file: File, codec: StreamCodec) -> io::Result<Box<dyn Read>> {
    match codec {
        StreamCodec::Gzip => Ok(Box::new(flate2::read::MultiGzDecoder::new(file))),
        StreamCodec::Zstd => zstd::decoder(file),
        StreamCodec::Xz => xz::decoder(file),
        StreamCodec::Lz4 => lz4::decoder(file),
        StreamCodec::Brotli => Ok(Box::new(brotli::Decompressor::new(file, 1 << 16))),
    }
}

/// Open a compressing writer over `sink` for the selected codec.
///
/// `sink` is any forward-only destination: the staging file of a file pack, the process stdout of
/// `pack --to-stdout`, or a test buffer.
pub fn encoder<W: Write + 'static>(
    sink: W,
    codec: StreamCodec,
    level: Option<u32>,
) -> io::Result<Box<dyn FinishEncoder>> {
    let level = validated_level(codec, level)?;
    match codec {
        StreamCodec::Gzip => Ok(Box::new(GzipEncoder(flate2::write::GzEncoder::new(
            sink,
            flate2::Compression::new(level.unwrap_or(6)),
        )))),
        StreamCodec::Zstd => zstd::encoder(sink, level),
        StreamCodec::Xz => xz::encoder(sink, level),
        StreamCodec::Lz4 => lz4::encoder(sink, level),
        StreamCodec::Brotli => Ok(Box::new(BrotliEncoder::new(sink, level.unwrap_or(9)))),
    }
}

struct GzipEncoder<W: Write>(flate2::write::GzEncoder<W>);

impl<W: Write> Write for GzipEncoder<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl<W: Write> FinishEncoder for GzipEncoder<W> {
    fn finish(self: Box<Self>) -> io::Result<()> {
        self.0.finish().map(drop)
    }
}

/// Sink that remembers the first write error, because the brotli writer reports a failed
/// final flush only through the sink it writes to.
struct ErrorTrackingSink<W: Write> {
    sink: W,
    failure: Option<(io::ErrorKind, String)>,
}

impl<W: Write> ErrorTrackingSink<W> {
    fn new(sink: W) -> Self {
        Self {
            sink,
            failure: None,
        }
    }

    fn record(&mut self, error: &io::Error) {
        if self.failure.is_none() {
            self.failure = Some((error.kind(), error.to_string()));
        }
    }
}

impl<W: Write> Write for ErrorTrackingSink<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.sink.write(buf) {
            Ok(written) => Ok(written),
            Err(error) => {
                self.record(&error);
                Err(error)
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.sink.flush() {
            Ok(()) => Ok(()),
            Err(error) => {
                self.record(&error);
                Err(error)
            }
        }
    }
}

struct BrotliEncoder<W: Write>(brotli::CompressorWriter<ErrorTrackingSink<W>>);

impl<W: Write> BrotliEncoder<W> {
    fn new(sink: W, quality: u32) -> Self {
        // 1 MiB output buffer keeps large members streaming; lgwin 22 is the format default.
        Self(brotli::CompressorWriter::new(
            ErrorTrackingSink::new(sink),
            1 << 20,
            quality,
            22,
        ))
    }
}

impl<W: Write> Write for BrotliEncoder<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl<W: Write> FinishEncoder for BrotliEncoder<W> {
    fn finish(mut self: Box<Self>) -> io::Result<()> {
        // `flush` reports write errors; the final trailer emitted by `into_inner` can only fail
        // through the sink, which keeps the first error for us to return here.
        self.0.flush()?;
        let sink = self.0.into_inner();
        match sink.failure {
            Some((kind, message)) => Err(io::Error::new(kind, message)),
            None => Ok(()),
        }
    }
}
