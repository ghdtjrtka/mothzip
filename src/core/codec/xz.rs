//! XZ stream backend: one `.xz` stream out, concatenated streams and stream padding in.
//!
//! `lzma_rust2` decodes one stream per reader and, in its own multi-stream mode, consumes any
//! number of trailing zero bytes at end of file. The XZ format allows padding only in whole
//! four-byte units, so concatenation is handled here: the backend starts a fresh reader per
//! stream, accepts only complete padding units between streams, and rejects everything else.
use super::FinishEncoder;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};

/// XZ stream header magic (`.xz` file format, stream header).
const XZ_MAGIC: [u8; 6] = [0xFD, b'7', b'z', b'X', b'Z', 0x00];
/// Stream padding is a whole number of four-byte zero units (XZ file format, stream padding).
const STREAM_PADDING_UNIT: usize = 4;

pub fn decoder(file: File) -> io::Result<Box<dyn Read>> {
    Ok(Box::new(ConcatenatedStreams::new(BufReader::new(file))))
}

pub fn encoder<W: Write + 'static>(
    sink: W,
    level: Option<u32>,
) -> io::Result<Box<dyn FinishEncoder>> {
    // Preset 6 is the default when `--level` is omitted; the factory already rejected any
    // out-of-range level, so this is the only preset decision the backend makes.
    let options = lzma_rust2::XzOptions::with_preset(level.unwrap_or(6));
    Ok(Box::new(XzEncoder(lzma_rust2::XzWriter::new(
        sink, options,
    )?)))
}

struct XzEncoder<W: Write>(lzma_rust2::XzWriter<W>);

impl<W: Write> Write for XzEncoder<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl<W: Write> FinishEncoder for XzEncoder<W> {
    fn finish(self: Box<Self>) -> io::Result<()> {
        // Writes the block trailer, index, and stream footer into the same sink.
        self.0.finish().map(drop)
    }
}

/// Decodes one stream, then every stream after it and the padding between them.
struct ConcatenatedStreams {
    stream: Option<lzma_rust2::XzReader<BufReader<File>>>,
}

impl ConcatenatedStreams {
    fn new(input: BufReader<File>) -> Self {
        // The first stream is required: an empty or corrupt file reports from the first read.
        Self {
            stream: Some(lzma_rust2::XzReader::new(input, false)),
        }
    }

    /// Skips whole padding units and returns the reader for the next stream, if one follows.
    ///
    /// A buffered read of a `File` returns every remaining byte, so a head shorter than a
    /// padding unit means the file ended before a complete unit or stream header.
    fn next_stream(
        mut input: BufReader<File>,
    ) -> io::Result<Option<lzma_rust2::XzReader<BufReader<File>>>> {
        loop {
            let head = input.fill_buf()?;
            if head.is_empty() {
                return Ok(None);
            }
            if head.len() < STREAM_PADDING_UNIT {
                return Err(trailing_bytes());
            }
            if head[..STREAM_PADDING_UNIT] == [0u8; STREAM_PADDING_UNIT] {
                input.consume(STREAM_PADDING_UNIT);
                continue;
            }
            if head.starts_with(&XZ_MAGIC) {
                return Ok(Some(lzma_rust2::XzReader::new(input, false)));
            }
            return Err(trailing_bytes());
        }
    }
}

impl Read for ConcatenatedStreams {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let Some(mut stream) = self.stream.take() else {
                return Ok(0);
            };
            match stream.read(buf) {
                Ok(0) => match Self::next_stream(stream.into_inner())? {
                    Some(next) => self.stream = Some(next),
                    None => return Ok(0),
                },
                Ok(read) => {
                    self.stream = Some(stream);
                    return Ok(read);
                }
                Err(error) => {
                    self.stream = Some(stream);
                    return Err(decode_error(error));
                }
            }
        }
    }
}

/// Truncation is `InvalidData` by contract; a real I/O failure keeps its own kind.
fn decode_error(error: io::Error) -> io::Error {
    if error.kind() == io::ErrorKind::UnexpectedEof {
        invalid_data("truncated XZ stream")
    } else {
        error
    }
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Bytes after the last stream that are neither a padding unit nor a stream header.
fn trailing_bytes() -> io::Error {
    invalid_data("trailing bytes after the last XZ stream")
}
