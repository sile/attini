//! Property-based tests for `attini::sansio::sse::SseDecoder`.

use attini::sansio::sse::{SseDecoder, SseError, SseEvent};

const ITERATIONS: usize = 256;
const SEED_ENV: &str = "ATTINI_PBT_SEED";

fn drain_all(decoder: &mut SseDecoder) -> (Vec<SseEvent>, Option<SseError>) {
    let mut events = Vec::new();
    loop {
        match decoder.next_event() {
            Ok(Some(event)) => events.push(event),
            Ok(None) => return (events, None),
            Err(err) => return (events, Some(err)),
        }
    }
}

fn feed_and_drain_chunked(bytes: &[u8], splits: &[usize]) -> (Vec<SseEvent>, Option<SseError>) {
    let mut decoder = SseDecoder::new();
    let mut events = Vec::new();
    let mut cursor = 0;
    let mut boundaries: Vec<usize> = splits.to_vec();
    boundaries.push(bytes.len());
    for boundary in boundaries {
        let end = boundary.min(bytes.len());
        if end < cursor {
            continue;
        }
        decoder.feed(&bytes[cursor..end]);
        cursor = end;
        loop {
            match decoder.next_event() {
                Ok(Some(event)) => events.push(event),
                Ok(None) => break,
                Err(err) => return (events, Some(err)),
            }
        }
    }
    (events, None)
}

fn sample_random_bytes(ctx: &mut noprop::TestCaseContext) -> Vec<u8> {
    let len = noprop::sample_usize_in(ctx, 0..=384);
    noprop::sample_bytes_vec(ctx, len)
}

fn sample_sse_like_bytes(ctx: &mut noprop::TestCaseContext) -> Vec<u8> {
    let events = noprop::sample_usize_in(ctx, 0..=6);
    let mut out = Vec::new();
    for _ in 0..events {
        let kind = noprop::sample_choice(ctx, &["data", "comment", "unknown_field", "blank"]);
        match kind {
            "data" => {
                let lines = noprop::sample_usize_in(ctx, 1..=3);
                for _ in 0..lines {
                    let len = noprop::sample_usize_in(ctx, 0..=16);
                    out.extend_from_slice(b"data: ");
                    out.extend_from_slice(
                        noprop::sample_ascii_printable_string(ctx, len).as_bytes(),
                    );
                    push_line_ending(ctx, &mut out);
                }
                push_line_ending(ctx, &mut out);
            }
            "comment" => {
                let len = noprop::sample_usize_in(ctx, 0..=16);
                out.extend_from_slice(b": ");
                out.extend_from_slice(noprop::sample_ascii_printable_string(ctx, len).as_bytes());
                push_line_ending(ctx, &mut out);
            }
            "unknown_field" => {
                let name = noprop::sample_choice(ctx, &["event", "id", "retry", "other"]);
                out.extend_from_slice(name.as_bytes());
                out.extend_from_slice(b": ");
                let len = noprop::sample_usize_in(ctx, 0..=8);
                out.extend_from_slice(noprop::sample_ascii_printable_string(ctx, len).as_bytes());
                push_line_ending(ctx, &mut out);
            }
            "blank" => {
                push_line_ending(ctx, &mut out);
            }
            _ => {}
        }
    }
    out
}

fn push_line_ending(ctx: &mut noprop::TestCaseContext, out: &mut Vec<u8>) {
    let ending = noprop::sample_choice(ctx, &["lf", "crlf", "cr"]);
    match ending {
        "lf" => out.push(b'\n'),
        "crlf" => out.extend_from_slice(b"\r\n"),
        "cr" => out.push(b'\r'),
        _ => out.push(b'\n'),
    }
}

fn sample_splits(ctx: &mut noprop::TestCaseContext, len: usize) -> Vec<usize> {
    let count = noprop::sample_usize_in(ctx, 0..=10);
    let mut splits = Vec::new();
    for _ in 0..count {
        splits.push(noprop::sample_usize_in(ctx, 0..=len));
    }
    splits.sort();
    splits.dedup();
    splits
}

fn assert_split_invariant(bytes: Vec<u8>, splits: Vec<usize>) {
    let mut baseline_decoder = SseDecoder::new();
    baseline_decoder.feed(&bytes);
    let baseline = drain_all(&mut baseline_decoder);
    let split = feed_and_drain_chunked(&bytes, &splits);
    assert_eq!(
        baseline.0, split.0,
        "event sequence differs (bytes={bytes:?}, splits={splits:?})"
    );
    match (baseline.1, split.1) {
        (None, None) => {}
        (Some(a), Some(b)) => assert_eq!(
            a, b,
            "terminal errors differ (bytes={bytes:?}, splits={splits:?})"
        ),
        (a, b) => panic!("presence of terminal error differs: baseline={a:?} split={b:?}"),
    }
}

#[test]
fn split_invariance_on_random_bytes() -> noprop::Result<()> {
    let seed = noprop::seed_from_env_or_time(SEED_ENV).expect("valid seed");
    noprop::Runner::new(seed, ITERATIONS).run(|ctx| {
        let bytes = sample_random_bytes(ctx);
        let splits = sample_splits(ctx, bytes.len());
        assert_split_invariant(bytes, splits);
        Ok(())
    })?;
    Ok(())
}

#[test]
fn split_invariance_on_sse_like_bytes() -> noprop::Result<()> {
    let seed = noprop::seed_from_env_or_time(SEED_ENV).expect("valid seed");
    noprop::Runner::new(seed, ITERATIONS).run(|ctx| {
        let bytes = sample_sse_like_bytes(ctx);
        let splits = sample_splits(ctx, bytes.len());
        assert_split_invariant(bytes, splits);
        Ok(())
    })?;
    Ok(())
}
