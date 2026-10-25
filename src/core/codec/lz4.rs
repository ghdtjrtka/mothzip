//! LZ4 stream backend: one LZ4 frame per stream, and every frame of a concatenated input.
//!
//! `lz4_flex::frame::FrameDecoder` reports the end of a frame as a zero-length read, exactly like
//! end of file, and reports a frame cut before its end mark as that same zero. The adapter below
//! therefore inspects the decoder's consumed byte count: a zero after a completed frame resumes
//! with the next frame, a zero while a frame is still open is a cut frame and becomes
//! `InvalidData`, and only a zero at a frame boundary with nothing left is end of file.
use super::FinishEncoder;
use lz4_flex::frame::{FrameDecoder, FrameEncoder, FrameInfo};
use std::fs::File;
use std::io::{self, ErrorKind, Read, Write};

pub fn decoder(file: File) -> io::Result<Box<dyn Read>> {
    Ok(Box::new(FrameStream {
        inner: FrameDecoder::new(Source {
            file,
            consumed: 0,
            at_eof: false,
        }),
        frames: 0,
        boundary: 0,
        at_boundary: true,
    }))
}

pub fn encoder<W: Write + 'static>(
    sink: W,
    _level: Option<u32>,
) -> io::Result<Box<dyn FinishEncoder>> {
    // A frame has no compression level, so `Some(level)` is rejected by `codec::encoder` before
    // this backend runs. The content checksum is the only whole-frame integrity check the format
    // has beyond the mandatory header checksum, so every frame written here carries one.
    let info = FrameInfo::new().content_checksum(true);
    Ok(Box::new(Lz4Encoder(FrameEncoder::with_frame_info(
        info, sink,
    ))))
}

struct Lz4Encoder<W: Write>(FrameEncoder<W>);

impl<W: Write> Write for Lz4Encoder<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl<W: Write> FinishEncoder for Lz4Encoder<W> {
    fn finish(self: Box<Self>) -> io::Result<()> {
        // Writes the pending block(s), the end mark and the content checksum into the same sink.
        self.0.finish().map(drop).map_err(io::Error::from)
    }
}

/// File reader that remembers how many compressed bytes the frame decoder consumed and whether
/// its last read reached end of file.
struct Source {
    file: File,
    consumed: u64,
    at_eof: bool,
}

impl Read for Source {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let read = self.file.read(buf)?;
        self.consumed += read as u64;
        self.at_eof = read == 0;
        Ok(read)
    }
}

struct FrameStream {
    inner: FrameDecoder<Source>,
    /// Frames decoded to completion so far.
    frames: u64,
    /// Compressed bytes consumed when the last frame ended.
    boundary: u64,
    /// Whether the decoder is between frames (nothing of the next frame consumed yet).
    at_boundary: bool,
}

impl Read for FrameStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            match self.inner.read(buf) {
                Ok(0) => {
                    let (at_eof, consumed) = {
                        let source = self.inner.get_ref();
                        (source.at_eof, source.consumed)
                    };
                    if at_eof {
                        if !self.at_boundary {
                            return Err(cut_frame("frame ended without an end mark"));
                        }
                        if consumed != self.boundary {
                            return Err(cut_frame("partial frame header at end of file"));
                        }
                        if self.frames == 0 {
                            return Err(cut_frame("input contains no LZ4 frame"));
                        }
                        return Ok(0);
                    }
                    // The frame ended: the next read decodes the next one, or sees end of file.
                    self.frames += 1;
                    self.at_boundary = true;
                    self.boundary = consumed;
                }
                Ok(read) => {
                    self.at_boundary = false;
                    return Ok(read);
                }
                Err(error) => return Err(bad_input(error)),
            }
        }
    }
}

/// A cut or structurally invalid frame is bad input, not a host I/O failure.
fn bad_input(error: io::Error) -> io::Error {
    let frame_error = error
        .get_ref()
        .is_some_and(|inner| inner.is::<lz4_flex::frame::Error>());
    // `UnexpectedEof` comes from `read_exact` on a cut frame; `lz4_flex` reports a corrupt block
    // or an unsupported frame feature as its own error, which it wraps in `ErrorKind::Other`.
    if frame_error || error.kind() == ErrorKind::UnexpectedEof {
        io::Error::new(ErrorKind::InvalidData, error)
    } else {
        error
    }
}

fn cut_frame(message: &str) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message)
}
