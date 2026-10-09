//! What [`run`] and [`run_streaming`] do with a large output: keep a head
//! and tail with a marker, stream it, or fail loud at the cap.

use super::*;

/// Run on a thread so a runner that hangs fails the test instead of the
/// suite.
#[cfg(unix)]
fn bounded<T: Send + 'static>(bound: Duration, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(bound)
        .expect("the runner returned within the bound")
}

#[cfg(unix)]
fn sh(script: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", script]);
    cmd
}

/// Output far past head plus tail, with the cap set low enough that holding
/// it whole would fail: the caller gets the head, the marker naming the
/// dropped bytes, the tail and the exit status; the child is not ended.
#[cfg(unix)]
#[test]
fn head_and_tail_keep_the_ends_with_a_marker_and_the_exit_status() {
    let mut limits = Limits::new(Duration::from_secs(30));
    limits.max_output = 10_000;
    limits.output = OutputMode::HeadTail {
        head: 100,
        tail: 200,
    };
    // 3 MB of 'a', then a distinct end.
    let got = bounded(Duration::from_secs(60), move || {
        run(
            sh("head -c 3000000 /dev/zero | tr '\\0' a; printf 'THE-END'; exit 4"),
            "probe",
            None,
            limits,
        )
    })
    .expect("a chatty child is not an error");
    assert_eq!(got.status.code(), Some(4));
    let text = String::from_utf8(got.stdout).expect("ascii");
    let dropped = 3_000_007 - 100 - 200;
    assert_eq!(got.omitted, [dropped, 0]);
    let marker = omitted_marker(dropped);
    assert!(
        marker.contains("MiB of output omitted from the middle"),
        "{marker}"
    );
    assert_eq!(
        text,
        format!("{}\n{marker}\n{}THE-END", "a".repeat(100), "a".repeat(193))
    );
    assert!(got.stderr.is_empty());
}

/// Output that fits head plus tail comes back as it was, with no marker.
#[cfg(unix)]
#[test]
fn head_and_tail_leave_a_short_output_alone() {
    let limits = Limits::new(Duration::from_secs(30)).keeping_head_and_tail();
    let got = bounded(Duration::from_secs(60), move || {
        run(sh("echo hello; echo oops >&2"), "probe", None, limits)
    })
    .expect("ran");
    assert_eq!(got.stdout, b"hello\n");
    assert_eq!(got.stderr, b"oops\n");
    assert_eq!(got.omitted, [0, 0]);
}

/// Output past the cap, read record by record: no cap applies to the whole,
/// every record arrives whole and in order, and the status is returned.
#[cfg(unix)]
#[test]
fn streaming_reads_an_output_larger_than_the_cap() {
    let mut limits = Limits::new(Duration::from_secs(30));
    limits.max_output = 10_000;
    let (finished, seen, bytes, in_order) = bounded(Duration::from_secs(120), move || {
        let (mut seen, mut bytes, mut in_order) = (0u64, 0u64, true);
        let got = run_streaming(
            // 200,000 lines: well over 1 MB, a hundred times the cap.
            sh("i=0; while [ $i -lt 200000 ]; do echo \"line-$i\"; i=$((i+1)); done; exit 2"),
            "probe",
            limits,
            b'\n',
            |record| {
                in_order &= record == format!("line-{seen}").as_bytes();
                seen += 1;
                bytes += record.len() as u64 + 1;
            },
        );
        (got, seen, bytes, in_order)
    });
    let finished = finished.expect("ran");
    assert_eq!(finished.status.code(), Some(2));
    assert_eq!(seen, 200_000);
    assert!(bytes > 100 * limits.max_output as u64, "{bytes}");
    assert!(in_order);
    assert!(finished.stdout.is_empty(), "nothing is held");
}

/// A record split across reads stays whole, and a last record with no
/// delimiter is delivered at the end.
#[test]
fn records_split_across_chunks_and_a_last_one_without_a_delimiter() {
    let mut records = sink::Records::new(0, 1000);
    let mut seen: Vec<Vec<u8>> = Vec::new();
    let mut on = |r: &[u8]| seen.push(r.to_vec());
    assert!(!records.push(b"ab\0c", &mut on));
    assert!(!records.push(b"d", &mut on));
    assert!(!records.push(b"e\0\0f", &mut on));
    records.finish(&mut on);
    assert_eq!(
        seen,
        vec![b"ab".to_vec(), b"cde".to_vec(), Vec::new(), b"f".to_vec()]
    );
}

/// One record longer than the cap is the named error: a stream with no
/// delimiter cannot make the reader hold it all.
#[cfg(unix)]
#[test]
fn streaming_a_record_past_the_cap_fails_loud() {
    let mut limits = Limits::new(Duration::from_secs(30));
    limits.max_output = 10_000;
    let err = bounded(Duration::from_secs(60), move || {
        run_streaming(
            sh("head -c 3000000 /dev/zero"),
            "probe",
            limits,
            b'\n',
            |_| {},
        )
    })
    .expect_err("one record of 3 MB");
    assert!(
        matches!(err, RunError::OutputTooLarge { cap: 10_000, .. }),
        "{err}"
    );
}

#[test]
fn head_and_tail_hold_what_the_sizes_say_across_many_pushes() {
    let mut kept = HeadTail::new(3, 5);
    for chunk in [&b"abc"[..], b"defg", b"hi", b"jklmnopq", b"r"] {
        kept.push(chunk);
    }
    assert_eq!(kept.omitted(), 18 - 3 - 5);
    let (bytes, omitted) = kept.finish();
    assert_eq!(omitted, 10);
    assert_eq!(
        String::from_utf8(bytes).expect("ascii"),
        format!("abc\n{}\nnopqr", omitted_marker(10))
    );
    let mut whole = HeadTail::new(3, 5);
    whole.push(b"abcdefgh");
    assert_eq!(whole.finish(), (b"abcdefgh".to_vec(), 0));
    assert_eq!(human_bytes(327_572_889), "312.4 MiB");
    assert_eq!(human_bytes(17), "17 bytes");
}

/// The default cap is the memory guard, 1 GiB, and a plain `run` holds.
#[test]
fn the_default_is_hold_under_a_one_gib_cap() {
    let limits = Limits::new(Duration::from_secs(1));
    assert_eq!(limits.max_output, 1 << 30);
    assert_eq!(limits.output, OutputMode::Hold);
}
