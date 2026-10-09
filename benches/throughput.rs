//! DATA delivery versus the same copy without protocol work. No sockets or TLS.

use std::hint::black_box;
use std::time::{Duration, Instant};

use micro_h2::frame::{DEFAULT_MAX_FRAME, HEADER_LEN, flags};
use micro_h2::{Connection, Event, FrameHeader, FrameType, frame};

const SAMPLES: usize = 7;
const STREAMING_BYTES: usize = 64 * 1024 * 1024;
const HOT_BYTES_PER_BATCH: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy)]
enum Path {
    Memcpy,
    RecvCopy,
    IncrementalCopy,
    Borrowed,
}

impl Path {
    const ALL: [Self; 4] = [
        Self::Memcpy,
        Self::RecvCopy,
        Self::IncrementalCopy,
        Self::Borrowed,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Memcpy => "memcpy",
            Self::RecvCopy => "recv + copy",
            Self::IncrementalCopy => "copy + finish_data",
            Self::Borrowed => "recv borrowed (protocol only)",
        }
    }
}

fn connection() -> Connection {
    let mut connection = Connection::new();
    let mut out = [0; 256];
    black_box(connection.start(&mut out).unwrap());
    let mut input = [0; 10];
    let len = frame::write_frame(FrameType::Settings, 0, 0, &[], &mut input).unwrap();
    connection.recv(&input[..len], |_, _| {}, &mut out).unwrap();
    let (stream, _) = connection
        .request("GET", "/data", "bench.test", "https", &[], &[], &mut out)
        .unwrap();
    assert_eq!(stream, 1);
    let len = frame::write_frame(
        FrameType::Headers,
        flags::END_HEADERS,
        stream,
        &[0x88], // :status: 200; streaming response, no Content-Length
        &mut input,
    )
    .unwrap();
    connection.recv(&input[..len], |_, _| {}, &mut out).unwrap();
    connection
}

struct Workload {
    connection: Connection,
    source: Vec<u8>,
    destination: Vec<u8>,
    out: [u8; 26],
    payload: usize,
    frames: usize,
    repeats: usize,
}

impl Workload {
    fn new(payload: usize, streaming: bool) -> Self {
        let frames = if streaming {
            STREAMING_BYTES / payload
        } else {
            1
        };
        let mut source = vec![0; frames * (HEADER_LEN + payload)];
        for (index, frame) in source.chunks_exact_mut(HEADER_LEN + payload).enumerate() {
            FrameHeader {
                length: payload,
                kind: FrameType::Data,
                flags: 0,
                stream: 1,
            }
            .write(frame)
            .unwrap();
            for (offset, byte) in frame[HEADER_LEN..].iter_mut().enumerate() {
                *byte = (index.wrapping_mul(31) ^ offset) as u8;
            }
        }
        Self {
            connection: connection(),
            source,
            destination: vec![0; frames * payload],
            out: [0; 26],
            payload,
            frames,
            repeats: if streaming {
                1
            } else {
                HOT_BYTES_PER_BATCH / payload
            },
        }
    }

    // Const specialization keeps path selection out of the timed frame loop.
    fn batch<const PATH: u8>(&mut self) -> usize {
        let source = black_box(self.source.as_slice());
        let destination = black_box(self.destination.as_mut_slice());
        for _ in 0..self.repeats {
            for (input, output) in source
                .chunks_exact(HEADER_LEN + self.payload)
                .zip(destination.chunks_exact_mut(self.payload))
            {
                // Identical per-frame compiler barriers on each delivery path.
                let input = black_box(input);
                let output = black_box(output);
                match PATH {
                    0 => output.copy_from_slice(&input[HEADER_LEN..]),
                    1 | 3 => {
                        let (event, written) = self
                            .connection
                            .recv(input, |_, _| {}, &mut self.out)
                            .unwrap();
                        match event {
                            Event::Data {
                                stream: 1,
                                data,
                                end_stream: false,
                            } => {
                                if PATH == 1 {
                                    output.copy_from_slice(data);
                                } else {
                                    black_box(data);
                                }
                            }
                            _ => panic!("expected streaming DATA"),
                        }
                        black_box(&self.out[..written]);
                    }
                    2 => {
                        let header = FrameHeader::parse(input).unwrap();
                        output.copy_from_slice(&input[HEADER_LEN..]);
                        let written = self.connection.finish_data(header, &mut self.out).unwrap();
                        black_box(&self.out[..written]);
                    }
                    _ => unreachable!(),
                }
                black_box(output);
            }
        }
        self.frames * self.payload * self.repeats
    }

    fn run(&mut self, path: Path) -> usize {
        match path {
            Path::Memcpy => self.batch::<0>(),
            Path::RecvCopy => self.batch::<1>(),
            Path::IncrementalCopy => self.batch::<2>(),
            Path::Borrowed => self.batch::<3>(),
        }
    }

    fn verify(&mut self) {
        for path in [Path::Memcpy, Path::RecvCopy, Path::IncrementalCopy] {
            self.destination.fill(0);
            self.run(path);
            for (input, output) in self
                .source
                .chunks_exact(HEADER_LEN + self.payload)
                .zip(self.destination.chunks_exact(self.payload))
            {
                assert_eq!(&input[HEADER_LEN..], output);
            }
        }
        let (event, _) = self
            .connection
            .recv(
                &self.source[..HEADER_LEN + self.payload],
                |_, _| {},
                &mut self.out,
            )
            .unwrap();
        let Event::Data { data, .. } = event else {
            panic!("expected borrowed DATA");
        };
        assert_eq!(data.as_ptr(), self.source[HEADER_LEN..].as_ptr());
        // Close the stream outside timing and verify its slot is released.
        let mut end = [0; HEADER_LEN];
        frame::write_frame(FrameType::Data, flags::END_STREAM, 1, &[], &mut end).unwrap();
        self.connection
            .recv(&end, |_, _| {}, &mut self.out)
            .unwrap();
        assert_eq!(self.connection.open_streams(), 0);
    }
}

fn measure(workload: &mut Workload, path: Path, minimum: Duration) -> f64 {
    let started = Instant::now();
    let mut bytes = 0u64;
    loop {
        bytes += workload.run(path) as u64;
        if started.elapsed() >= minimum {
            break;
        }
    }
    started.elapsed().as_secs_f64() / bytes as f64
}

fn main() {
    let quick = std::env::args().any(|arg| arg == "--quick" || arg == "--test");
    let minimum = Duration::from_millis(if quick { 10 } else { 100 });
    let samples = if quick { 3 } else { SAMPLES };
    println!("DATA delivery; median of {samples} samples; payload GB/s (decimal)");
    println!("Same source/destination, frame size, and compiler barriers for copy paths.");
    println!("Borrowed results measure protocol work, without reading/copying the body.\n");
    for streaming in [false, true] {
        for payload in [1024, 4096, DEFAULT_MAX_FRAME] {
            let mut workload = Workload::new(payload, streaming);
            for path in Path::ALL {
                workload.run(path); // page faults and warmup outside measurement
            }
            let mut timings = [[0.0; 4]; SAMPLES];
            for (sample, times) in timings.iter_mut().take(samples).enumerate() {
                // Rotate order to avoid giving one path every first/last sample.
                for offset in 0..4 {
                    let index = (sample + offset) % 4;
                    times[index] = measure(&mut workload, Path::ALL[index], minimum);
                }
            }
            let medians: [f64; 4] = std::array::from_fn(|index| {
                let mut times = timings.map(|sample| sample[index]);
                times[..samples].sort_by(f64::total_cmp);
                times[samples / 2]
            });
            let working_set = workload.source.len() + workload.destination.len();
            println!(
                "{}: {payload} B/frame; {working_set} B source + destination",
                if streaming {
                    "64 MiB sweep"
                } else {
                    "hot buffers"
                }
            );
            for (index, path) in Path::ALL.into_iter().enumerate() {
                let rate = 1.0 / medians[index] / 1e9;
                let ns = medians[index] * payload as f64 * 1e9;
                if matches!(path, Path::Borrowed) {
                    println!("  {:30} {ns:8.1} ns/frame", path.name());
                } else {
                    let ratio = medians[0] / medians[index];
                    println!(
                        "  {:30} {rate:8.2} GB/s  {ns:8.1} ns/frame  {ratio:5.2}x memcpy",
                        path.name()
                    );
                }
            }
            workload.verify();
            println!();
        }
    }
}
