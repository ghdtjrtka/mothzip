//! Zstd stream backend.
//!
//! One encoded file is exactly one zstd frame. A decoded file may hold several concatenated
//! frames, because the format defines them as one stream.
use super::FinishEncoder;
use std::fs::File;
use std::io::{self, Read, Write};

/// `ZSTD_CLEVEL_DEFAULT`: the level zstd itself uses when none is requested (`zstd -3`).
const DEFAULT_LEVEL: u32 = 3;

pub fn decoder(file: File) -> io::Result<Box<dyn Read>> {
    // The crate's reader decodes frames until EOF and starts a new frame after each frame end,
    // so concatenated frames are decoded and bytes after the last frame reach zstd as a frame
    // header and fail there instead of being ignored.
    let decoder = zstd::stream::read::Decoder::new(file)?;
    Ok(Box::new(StrictDecoder(decoder)))
}

pub fn encoder<W: Write + 'static>(
    sink: W,
    level: Option<u32>,
) -> io::Result<Box<dyn FinishEncoder>> {
    // `validated_level` owns the 1..=22 range; only the documented default is applied here.
    let level = level.unwrap_or(DEFAULT_LEVEL) as i32;
    let mut encoder = zstd::stream::write::Encoder::new(sink, level)?;
    // A frame without a content checksum decodes a flipped payload byte into silently wrong
    // output, so the decoder cannot reject corruption. zstd's CLI records the checksum by
    // default (`--no-check` disables it); do the same so corruption stays detectable.
    encoder.include_checksum(true)?;
    Ok(Box::new(ZstdEncoder(encoder)))
}

/// Decoder that reports every stream fault as `InvalidData`.
struct StrictDecoder(zstd::stream::read::Decoder<'static, io::BufReader<File>>);

impl Read for StrictDecoder {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf).map_err(stream_error)
    }
}

/// The crate turns zstd error codes into `Other` and a frame cut short into `UnexpectedEof`;
/// the stream contract reports both as `InvalidData` so a short stream never looks like success.
fn stream_error(error: io::Error) -> io::Error {
    match error.kind() {
        io::ErrorKind::UnexpectedEof | io::ErrorKind::Other => {
            io::Error::new(io::ErrorKind::InvalidData, error)
        }
        _ => error,
    }
}

struct ZstdEncoder<W: Write>(zstd::stream::write::Encoder<'static, W>);

impl<W: Write> Write for ZstdEncoder<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl<W: Write> FinishEncoder for ZstdEncoder<W> {
    fn finish(self: Box<Self>) -> io::Result<()> {
        self.0.finish().map(drop)
    }
}
