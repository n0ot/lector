use lector::tmux_control::{ControlEvent, ParserLimits, TmuxControlParser};

const START: &[u8] = b"\x1bP1000p";
const END: &[u8] = b"\x1b\\";

#[derive(Clone, Copy)]
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut value = self.0;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.0 = value;
        value
    }

    fn usize(&mut self, exclusive_max: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(exclusive_max).unwrap()).unwrap()
    }

    fn byte(&mut self) -> u8 {
        self.next().to_le_bytes()[0]
    }
}

fn push_in_random_chunks(
    parser: &mut TmuxControlParser,
    input: &[u8],
    rng: &mut Rng,
) -> Result<Vec<ControlEvent>, ()> {
    let mut events = Vec::new();
    let mut offset = 0;
    while offset < input.len() {
        let count = 1 + rng.usize((input.len() - offset).min(31));
        match parser.push(&input[offset..offset + count]) {
            Ok(next) => events.extend(next),
            Err(_) => return Err(()),
        }
        offset += count;
    }
    match parser.finish() {
        Ok(next) => {
            events.extend(next);
            Ok(events)
        }
        Err(_) => Err(()),
    }
}

fn encode_octal(bytes: &[u8], encoded: &mut Vec<u8>) {
    for &byte in bytes {
        if (0x20..=0x7e).contains(&byte) && byte != b'\\' {
            encoded.push(byte);
        } else {
            encoded.extend_from_slice(format!("\\{byte:03o}").as_bytes());
        }
    }
}

#[test]
fn deterministic_generated_stream_fuzz_preserves_binary_output_and_fragmentation() {
    let mut rng = Rng(0x4c45_4354_4f52_465a);
    for case in 0..5_000 {
        let pane_id = rng.next() % 10_000;
        let payload_len = rng.usize(96);
        let payload = (0..payload_len).map(|_| rng.byte()).collect::<Vec<_>>();
        let mut stream = START.to_vec();
        stream.extend_from_slice(format!("%begin {case} {case} 0\n").as_bytes());
        stream.extend_from_slice(b"command output\n");
        stream.extend_from_slice(format!("%end {case} {case} 0\n").as_bytes());
        stream.extend_from_slice(format!("%output %{pane_id} ").as_bytes());
        encode_octal(&payload, &mut stream);
        stream.extend_from_slice(b"\n%exit fuzz complete\n");
        stream.extend_from_slice(END);

        let expected = {
            let mut parser = TmuxControlParser::new();
            let mut events = parser.push(&stream).unwrap();
            events.extend(parser.finish().unwrap());
            events
        };
        let output = expected
            .iter()
            .find_map(|event| match event {
                ControlEvent::Output { bytes, .. } => Some(bytes),
                _ => None,
            })
            .unwrap();
        assert_eq!(output, &payload, "binary output changed in case {case}");

        for _ in 0..3 {
            let mut parser = TmuxControlParser::new();
            assert_eq!(
                push_in_random_chunks(&mut parser, &stream, &mut rng),
                Ok(expected.clone()),
                "fragmentation changed generated case {case}"
            );
        }
    }
}

#[test]
fn deterministic_arbitrary_byte_fuzz_never_panics_or_retains_an_unbounded_record() {
    let limits = ParserLimits {
        max_line_bytes: 128,
        max_command_output_bytes: 256,
        max_command_output_lines: 16,
        max_notification_bytes: 96,
    };
    let mut rng = Rng(0x544d_5558_4259_5445);

    for _case in 0..25_000 {
        let length = rng.usize(512);
        let mut input = (0..length).map(|_| rng.byte()).collect::<Vec<_>>();
        if rng.usize(2) == 0 {
            input.splice(0..0, START.iter().copied());
        }

        let mut parser = TmuxControlParser::with_limits(limits);
        let mut offset = 0;
        let mut failed = false;
        while offset < input.len() {
            let count = 1 + rng.usize((input.len() - offset).min(47));
            if parser.push(&input[offset..offset + count]).is_err() {
                failed = true;
                break;
            }
            offset += count;
        }
        if !failed {
            let _ = parser.finish();
        }

        parser.reset();
        assert!(parser.push(b"\x1bP1000p%exit\n\x1b\\").is_ok());
        assert!(parser.finish().is_ok());
    }
}
