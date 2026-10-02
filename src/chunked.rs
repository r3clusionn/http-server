//! Chunked transfer coding: an incremental decoder for request bodies and a writer for responses.

use std::io::{self, Write};

use crate::request::ParseError;

fn err(status: u16, reason: &'static str) -> ParseError {
    ParseError { status, reason }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Reading hexadecimal digits of the chunk size.
    Size,
    /// After `;`: chunk extensions, ignored up to the end of the line.
    Extension,
    /// Saw the CR that ends the size line.
    SizeLf,
    /// Inside chunk data, with this many bytes left.
    Data(u64),
    /// Expecting the CR after chunk data.
    DataCr,
    DataLf,
    /// At the start of a trailer line (or the final empty line).
    TrailerStart,
    /// Inside a trailer line.
    Trailer,
    TrailerLf,
    /// The CR of the empty line that ends the body was seen.
    FinalLf,
    Done,
}

/// Decodes a chunked body as bytes arrive. The result is the same however the input is split.
#[derive(Debug)]
pub struct Decoder {
    state: State,
    size: u64,
    digits: usize,
    total: usize,
    trailer_bytes: usize,
    max_body: usize,
}

const MAX_SIZE_DIGITS: usize = 16;
const MAX_TRAILER_BYTES: usize = 8 * 1024;

impl Decoder {
    pub fn new(max_body: usize) -> Decoder {
        Decoder { state: State::Size, size: 0, digits: 0, total: 0, trailer_bytes: 0, max_body }
    }

    pub fn is_done(&self) -> bool {
        self.state == State::Done
    }

    /// Consumes bytes from `input`, appending decoded data to `out`. Returns how many bytes of
    /// `input` were used; when the body is complete this is less than `input.len()` if the input
    /// held the start of the next request.
    pub fn feed(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<usize, ParseError> {
        let mut i = 0;
        while i < input.len() && self.state != State::Done {
            let b = input[i];
            match self.state {
                State::Size => match b {
                    b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F' => {
                        self.digits += 1;
                        if self.digits > MAX_SIZE_DIGITS {
                            return Err(err(400, "chunk size too long"));
                        }
                        self.size = self.size * 16 + (b as char).to_digit(16).unwrap() as u64;
                        i += 1;
                    }
                    b';' if self.digits > 0 => {
                        self.state = State::Extension;
                        i += 1;
                    }
                    b'\r' if self.digits > 0 => {
                        self.state = State::SizeLf;
                        i += 1;
                    }
                    _ => return Err(err(400, "bad chunk size")),
                },
                State::Extension => {
                    match b {
                        b'\r' => self.state = State::SizeLf,
                        b'\n' => return Err(err(400, "bare line feed in chunk header")),
                        _ => {}
                    }
                    i += 1;
                }
                State::SizeLf => {
                    if b != b'\n' {
                        return Err(err(400, "bad chunk header line ending"));
                    }
                    i += 1;
                    self.state = if self.size == 0 {
                        State::TrailerStart
                    } else {
                        if self.total as u64 + self.size > self.max_body as u64 {
                            return Err(err(413, "request body too large"));
                        }
                        State::Data(self.size)
                    };
                }
                State::Data(left) => {
                    let n = (input.len() - i).min(left.min(usize::MAX as u64) as usize);
                    out.extend_from_slice(&input[i..i + n]);
                    self.total += n;
                    i += n;
                    let left = left - n as u64;
                    self.state = if left == 0 { State::DataCr } else { State::Data(left) };
                }
                State::DataCr => {
                    if b != b'\r' {
                        return Err(err(400, "chunk data not followed by CRLF"));
                    }
                    i += 1;
                    self.state = State::DataLf;
                }
                State::DataLf => {
                    if b != b'\n' {
                        return Err(err(400, "chunk data not followed by CRLF"));
                    }
                    i += 1;
                    self.size = 0;
                    self.digits = 0;
                    self.state = State::Size;
                }
                State::TrailerStart => {
                    if b == b'\r' {
                        self.state = State::FinalLf;
                        i += 1;
                    } else {
                        self.state = State::Trailer;
                    }
                }
                State::Trailer => {
                    self.trailer_bytes += 1;
                    if self.trailer_bytes > MAX_TRAILER_BYTES {
                        return Err(err(431, "trailer too large"));
                    }
                    match b {
                        b'\r' => self.state = State::TrailerLf,
                        b'\n' => return Err(err(400, "bare line feed in trailer")),
                        _ => {}
                    }
                    i += 1;
                }
                State::TrailerLf => {
                    if b != b'\n' {
                        return Err(err(400, "bad trailer line ending"));
                    }
                    i += 1;
                    self.state = State::TrailerStart;
                }
                State::FinalLf => {
                    if b != b'\n' {
                        return Err(err(400, "bad end of chunked body"));
                    }
                    i += 1;
                    self.state = State::Done;
                }
                State::Done => unreachable!(),
            }
        }
        Ok(i)
    }
}

/// Writes a chunked body to an underlying writer.
pub struct ChunkWriter<'a, W: Write + ?Sized> {
    inner: &'a mut W,
}

impl<'a, W: Write + ?Sized> ChunkWriter<'a, W> {
    pub fn new(inner: &'a mut W) -> Self {
        ChunkWriter { inner }
    }

    /// Writes the terminating chunk.
    pub fn finish(self) -> io::Result<()> {
        self.inner.write_all(b"0\r\n\r\n")
    }
}

impl<W: Write + ?Sized> Write for ChunkWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // An empty chunk would end the body, so empty writes are dropped.
        if buf.is_empty() {
            return Ok(0);
        }
        write!(self.inner, "{:x}\r\n", buf.len())?;
        self.inner.write_all(buf)?;
        self.inner.write_all(b"\r\n")?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests;
