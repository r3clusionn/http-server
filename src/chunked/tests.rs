use super::*;

fn decode_all(input: &[u8]) -> Result<(Vec<u8>, usize), ParseError> {
    let mut d = Decoder::new(1 << 20);
    let mut out = Vec::new();
    let n = d.feed(input, &mut out)?;
    assert!(d.is_done() || n == input.len());
    Ok((out, n))
}

#[test]
fn decodes_a_simple_body() {
    let (out, n) = decode_all(b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\nNEXT").unwrap();
    assert_eq!(out, b"hello world");
    assert_eq!(n, 26, "the bytes after the body are not consumed");
}

#[test]
fn extensions_trailers_and_hex_case() {
    let (out, _) = decode_all(b"A;name=value\r\n0123456789\r\nb\r\nabcdefghijk\r\n0;done\r\nX-Sum: 1\r\nY: 2\r\n\r\n").unwrap();
    assert_eq!(out, b"0123456789abcdefghijk");
}

#[test]
fn same_result_for_every_split_point() {
    let msg = b"4\r\nWiki\r\n5;x=y\r\npedia\r\nE\r\n in\r\n\r\nchunks.\r\n0\r\nTrailer: v\r\n\r\n";
    let (whole, n) = decode_all(msg).unwrap();
    assert_eq!(whole, b"Wikipedia in\r\n\r\nchunks.");
    assert_eq!(n, msg.len());
    for cut in 0..=msg.len() {
        let mut d = Decoder::new(1 << 20);
        let mut out = Vec::new();
        let a = d.feed(&msg[..cut], &mut out).unwrap();
        assert_eq!(a, cut);
        let b = d.feed(&msg[cut..], &mut out).unwrap();
        assert_eq!(a + b, msg.len());
        assert!(d.is_done());
        assert_eq!(out, whole, "split at {cut}");
    }
    // One byte at a time.
    let mut d = Decoder::new(1 << 20);
    let mut out = Vec::new();
    for b in msg.iter() {
        d.feed(std::slice::from_ref(b), &mut out).unwrap();
    }
    assert!(d.is_done() && out == whole);
}

#[test]
fn malformed_bodies_are_rejected() {
    let bad: &[&[u8]] = &[
        b"\r\n0\r\n\r\n",            // no size
        b"g\r\nx\r\n0\r\n\r\n",      // not hex
        b"-1\r\n",                   // sign
        b" 5\r\nhello\r\n",          // space before size
        b"5 \r\nhello\r\n0\r\n\r\n", // space after size
        b"5\nhello\r\n",             // bare LF
        b"5\r\nhelloXX0\r\n\r\n",    // data not followed by CRLF
        b"5\r\nhello\n0\r\n\r\n",    // bare LF after data
        b"0\r\nTrailer\n\r\n",       // bare LF in trailer
        b"0\r\n\rX",                 // bad final line
        b"11111111111111111\r\n",    // 17 digits
    ];
    for b in bad {
        assert_eq!(decode_all(b).unwrap_err().status, 400, "{:?}", String::from_utf8_lossy(b));
    }
}

#[test]
fn body_limit_is_enforced_before_the_data_is_read() {
    let mut d = Decoder::new(10);
    let mut out = Vec::new();
    assert!(d.feed(b"5\r\nhello\r\n", &mut out).is_ok());
    let e = d.feed(b"6\r\n", &mut out).unwrap_err();
    assert_eq!(e.status, 413);
    // A huge declared size is refused at once, without waiting for its data.
    let mut d = Decoder::new(1000);
    assert_eq!(d.feed(b"ffffffff\r\n", &mut Vec::new()).unwrap_err().status, 413);
    // The largest representable size must not overflow.
    let mut d = Decoder::new(1000);
    assert_eq!(d.feed(b"ffffffffffffffff\r\n", &mut Vec::new()).unwrap_err().status, 413);
}

#[test]
fn oversized_trailers_are_refused() {
    let mut msg = b"0\r\n".to_vec();
    msg.extend(std::iter::repeat_n(b'x', MAX_TRAILER_BYTES + 10));
    assert_eq!(decode_all(&msg).unwrap_err().status, 431);
}

#[test]
fn writer_round_trips_through_the_decoder() {
    let mut wire = Vec::new();
    {
        let mut w = ChunkWriter::new(&mut wire);
        w.write_all(b"hello ").unwrap();
        w.write_all(b"").unwrap();
        w.write_all(&[7u8; 300]).unwrap();
        w.finish().unwrap();
    }
    assert!(wire.starts_with(b"6\r\nhello \r\n12c\r\n"));
    let (out, n) = decode_all(&wire).unwrap();
    assert_eq!(n, wire.len());
    assert_eq!(&out[..6], b"hello ");
    assert_eq!(out.len(), 306);
}

#[test]
fn random_input_never_panics() {
    let mut seed = 12345u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let alphabet = b"0123456789abcdefABCDEF;\r\n :xyz-";
    for _ in 0..20_000 {
        let n = (next() % 60) as usize;
        let v: Vec<u8> = (0..n).map(|_| alphabet[(next() as usize) % alphabet.len()]).collect();
        let mut d = Decoder::new(100);
        let mut out = Vec::new();
        if let Ok(used) = d.feed(&v, &mut out) {
            assert!(used <= v.len() && out.len() <= 100);
        }
    }
}
