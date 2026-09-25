mod common;

mod tests {
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

    use crate::common::TempDir;
    use error_stack::Report;
    use tarseer::WalkOptions;
    use tarseer::frames::{Codec, Encoder, FrameKind, ReadError, WriteError, write_frames};

    #[derive(Clone)]
    struct CountingCodec(Arc<(Mutex<usize>, Condvar)>);

    impl Codec for CountingCodec {
        fn encoder(&self) -> Result<Box<dyn Encoder>, Report<WriteError>> {
            Ok(Box::new(self.clone()))
        }
        fn decode(&self, frame: &[u8]) -> Result<Vec<u8>, Report<ReadError>> {
            Ok(frame.to_vec())
        }
    }

    impl Encoder for CountingCodec {
        fn encode(&mut self, raw: &[u8], frame: &mut Vec<u8>) -> Result<(), Report<WriteError>> {
            frame.clear();
            frame.extend_from_slice(raw);
            let (count, ready) = &*self.0;
            *count.lock().unwrap() += 1;
            ready.notify_all();
            Ok(())
        }
    }

    #[test]
    fn blocked_frame_sink_bounds_encoding_ahead() {
        let fixture = TempDir::new("backpressure");
        let root = fixture.path();
        for index in 0..100 {
            std::fs::write(root.join(format!("f{index:03}")), b"x").unwrap();
        }
        let codec = CountingCodec(Arc::new((Mutex::new(0), Condvar::new())));
        let mut encoded_while_blocked = 0;
        let options = WalkOptions {
            budget: 1,
            ..WalkOptions::default()
        };
        let index = write_frames(root, &options, &codec, 2, &mut |frame| {
            if frame.kind == FrameKind::Part(0) {
                let (count, ready) = &*codec.0;
                let (count, _) = ready
                    .wait_timeout_while(count.lock().unwrap(), Duration::from_secs(2), |count| {
                        *count < 100
                    })
                    .unwrap();
                encoded_while_blocked = *count;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(index.parts.len(), 100);
        eprintln!(
            "2 encoders, 2 queued jobs, first frame sink blocked: {encoded_while_blocked} of 100 parts encoded"
        );
        assert!(
            encoded_while_blocked <= 4,
            "the output queue retains the entire encoded manifest while the sink is blocked"
        );
    }

    struct CountingProgress(Arc<(Mutex<usize>, Condvar)>);

    struct PanickingCodec;

    impl Codec for PanickingCodec {
        fn encoder(&self) -> Result<Box<dyn Encoder>, Report<WriteError>> {
            Ok(Box::new(Self))
        }

        fn decode(&self, _: &[u8]) -> Result<Vec<u8>, Report<ReadError>> {
            unreachable!()
        }
    }

    impl Encoder for PanickingCodec {
        fn encode(&mut self, raw: &[u8], frame: &mut Vec<u8>) -> Result<(), Report<WriteError>> {
            assert!(
                !raw.windows(6).any(|text| text == b"\"f000\""),
                "encoder panic"
            );
            frame.clear();
            frame.extend_from_slice(raw);
            Ok(())
        }
    }

    #[test]
    #[should_panic(expected = "a scoped thread panicked")]
    fn encoder_panic_wakes_a_walk_waiting_for_admission() {
        let fixture = TempDir::new("encoding-panic");
        for index in 0..100 {
            std::fs::write(fixture.path().join(format!("f{index:03}")), b"x").unwrap();
        }
        let options = WalkOptions {
            budget: 1,
            ..WalkOptions::default()
        };
        let _ = write_frames(
            fixture.path(),
            &options,
            &PanickingCodec,
            2,
            &mut |_| Ok(()),
        );
    }

    #[derive(Clone)]
    struct SlowFirstCodec {
        counting: CountingCodec,
        ahead: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Codec for SlowFirstCodec {
        fn encoder(&self) -> Result<Box<dyn Encoder>, Report<WriteError>> {
            Ok(Box::new(self.clone()))
        }

        fn decode(&self, frame: &[u8]) -> Result<Vec<u8>, Report<ReadError>> {
            Ok(frame.to_vec())
        }
    }

    impl Encoder for SlowFirstCodec {
        fn encode(&mut self, raw: &[u8], frame: &mut Vec<u8>) -> Result<(), Report<WriteError>> {
            self.counting.encode(raw, frame)?;
            if raw.windows(6).any(|text| text == b"\"f000\"")
                && self.ahead.load(std::sync::atomic::Ordering::Relaxed) == 0
            {
                let (count, ready) = &*self.counting.0;
                let (count, _) = ready
                    .wait_timeout_while(count.lock().unwrap(), Duration::from_secs(2), |count| {
                        *count < 100
                    })
                    .unwrap();
                self.ahead
                    .store(*count, std::sync::atomic::Ordering::Relaxed);
            }
            Ok(())
        }
    }

    #[test]
    fn slow_first_encoder_bounds_reordering_ahead() {
        let fixture = TempDir::new("encoding-pressure");
        for index in 0..100 {
            std::fs::write(fixture.path().join(format!("f{index:03}")), b"x").unwrap();
        }
        let codec = SlowFirstCodec {
            counting: CountingCodec(Arc::new((Mutex::new(0), Condvar::new()))),
            ahead: Arc::default(),
        };
        let options = WalkOptions {
            budget: 1,
            ..WalkOptions::default()
        };
        let index = write_frames(fixture.path(), &options, &codec, 2, &mut |_| Ok(())).unwrap();
        assert_eq!(index.parts.len(), 100);
        let ahead = codec.ahead.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            (1..=4).contains(&ahead),
            "{ahead} parts encoded before part zero finished"
        );
    }

    impl tarseer::Progress for CountingProgress {
        fn recorded(&self, _: tarseer::EntryKind, _: u64) {
            let (count, ready) = &*self.0;
            *count.lock().unwrap() += 1;
            ready.notify_all();
        }
    }

    #[test]
    fn blocked_walk_sink_bounds_reading_ahead() {
        const TOTAL: usize = 64 * 257;
        // Soft row window plus active scan quanta and directory scans.
        const MAX_AHEAD: usize = 2 + 2 * 2048 + 2 * (2048 + 1024);
        let fixture = TempDir::new("walk-pressure");
        let root = fixture.path();
        for directory in 0..64 {
            let leaf = root.join(format!("d{directory:03}"));
            std::fs::create_dir_all(&leaf).unwrap();
            for index in 0..256 {
                std::fs::write(leaf.join(format!("f{index:03}")), b"x").unwrap();
            }
        }
        {
            let progress = CountingProgress(Arc::new((Mutex::new(0), Condvar::new())));
            let options = WalkOptions {
                budget: 1,
                threads: 2,
                progress: Some(&progress),
                ..WalkOptions::default()
            };
            let mut first = true;
            let mut read_while_blocked = 0;
            let mut received = 0;
            tarseer::walk_parts(root, &options, &mut |part| {
                received += part.len();
                if first {
                    first = false;
                    let (count, ready) = &*progress.0;
                    let (count, _) = ready
                        .wait_timeout_while(
                            count.lock().unwrap(),
                            Duration::from_secs(5),
                            |count| *count < TOTAL,
                        )
                        .unwrap();
                    read_while_blocked = *count;
                }
                Ok(())
            })
            .unwrap();
            assert_eq!(received, TOTAL);
            eprintln!(
                "2 walk threads, budget 1, first sink blocked: {read_while_blocked} of {TOTAL} entries recorded (row window 4098)"
            );
            // The soft row window plus each active worker's scan quantum
            // and one directory scan, without the wide-listing exception.
            assert!(
                read_while_blocked <= MAX_AHEAD,
                "{read_while_blocked} rows exceeded the bounded read-ahead window"
            );
        }
    }
}
