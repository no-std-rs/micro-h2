//! A scripted, flow-controlled peer and client entirely inside a 50,000-byte
//! workload heap budget. Verifies every body byte; no socket/TLS/executor costs.

#[path = "support/allocator.rs"]
mod allocator;

use std::hint::black_box;
use std::mem::size_of;
use std::time::Instant;

use micro_h2::conn::{MAX_STREAMS, RECEIVE_WINDOW};
use micro_h2::frame::{DEFAULT_MAX_FRAME, HEADER_LEN, flags, settings};
use micro_h2::hpack::encode::encode_header;
use micro_h2::{Connection, Event, FrameHeader, FrameType, frame};

#[global_allocator]
static ALLOCATOR: allocator::Meter = allocator::Meter;

const BUDGET: usize = 50_000; // decimal kB, stricter than 50 KiB
const BODY: usize = 4 * 1024 * 1024;
const TX: usize = 2048;
const CHUNK: usize = 512;
const FRAME_BUFFER: usize = HEADER_LEN + DEFAULT_MAX_FRAME;

fn pattern(stream: u32, offset: usize) -> u8 {
    (offset.wrapping_mul(31) ^ (offset >> 8) ^ stream as usize) as u8
}

struct Peer {
    streams: [u32; MAX_STREAMS],
    connection_credit: usize,
    stream_credit: [usize; MAX_STREAMS],
    received: [usize; MAX_STREAMS],
    credited: [usize; MAX_STREAMS],
    connection_credited: usize,
    updates: usize,
    indexed: bool,
}

impl Peer {
    fn new() -> Self {
        Self {
            streams: [0; MAX_STREAMS],
            connection_credit: RECEIVE_WINDOW as usize,
            stream_credit: [RECEIVE_WINDOW as usize; MAX_STREAMS],
            received: [0; MAX_STREAMS],
            credited: [0; MAX_STREAMS],
            connection_credited: 0,
            updates: 0,
            indexed: false,
        }
    }

    fn consume(&mut self, index: usize, length: usize) {
        assert!(self.connection_credit >= length, "connection stalled");
        assert!(self.stream_credit[index] >= length, "stream stalled");
        self.connection_credit -= length;
        self.stream_credit[index] -= length;
    }

    fn replies(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let header = FrameHeader::parse(bytes).unwrap();
            assert_eq!(header.kind, FrameType::WindowUpdate);
            assert_eq!(header.length, 4);
            let credit = u32::from_be_bytes(bytes[HEADER_LEN..HEADER_LEN + 4].try_into().unwrap());
            assert!(credit > 0 && credit <= 0x7fff_ffff);
            if header.stream == 0 {
                self.connection_credit += credit as usize;
                self.connection_credited += credit as usize;
                assert!(self.connection_credit <= RECEIVE_WINDOW as usize);
            } else {
                let index = self
                    .streams
                    .iter()
                    .position(|id| *id == header.stream)
                    .unwrap();
                self.stream_credit[index] += credit as usize;
                self.credited[index] += credit as usize;
                assert!(self.stream_credit[index] <= RECEIVE_WINDOW as usize);
            }
            self.updates += 1;
            bytes = &bytes[HEADER_LEN + 4..];
        }
    }
}

struct Workspace<const RX: usize, const SINK: usize> {
    connection: Connection,
    input: [u8; RX],
    sink: [u8; SINK],
    output: [u8; TX],
    peer: Peer,
}

const _: () = assert!(size_of::<Workspace<FRAME_BUFFER, DEFAULT_MAX_FRAME>>() <= BUDGET);
const _: () = assert!(size_of::<Workspace<CHUNK, CHUNK>>() <= BUDGET);

impl<const RX: usize, const SINK: usize> Workspace<RX, SINK> {
    fn new() -> Self {
        Self {
            connection: Connection::new(),
            input: [0; RX],
            sink: [0; SINK],
            output: [0; TX],
            peer: Peer::new(),
        }
    }

    fn start(&mut self) {
        let written = self.connection.start(&mut self.output).unwrap();
        assert_eq!(
            &self.output[..frame::CLIENT_PREFACE.len()],
            frame::CLIENT_PREFACE
        );
        let settings_start = frame::CLIENT_PREFACE.len();
        let header = FrameHeader::parse(&self.output[settings_start..]).unwrap();
        assert_eq!(header.kind, FrameType::Settings);
        let settings_end = settings_start + HEADER_LEN + header.length;
        for setting in self.output[settings_start + HEADER_LEN..settings_end]
            .as_chunks::<6>()
            .0
        {
            let id = u16::from_be_bytes(setting[..2].try_into().unwrap());
            let value = u32::from_be_bytes(setting[2..].try_into().unwrap());
            if id == settings::INITIAL_WINDOW_SIZE {
                assert_eq!(value, RECEIVE_WINDOW);
            }
        }
        let update = FrameHeader::parse(&self.output[settings_end..written]).unwrap();
        assert_eq!(update.kind, FrameType::WindowUpdate);
        assert_eq!(
            u32::from_be_bytes(
                self.output[settings_end + HEADER_LEN..written]
                    .try_into()
                    .unwrap()
            ),
            RECEIVE_WINDOW - 65_535
        );
        let len = frame::write_frame(FrameType::Settings, 0, 0, &[], &mut self.input).unwrap();
        let (event, written) = self
            .connection
            .recv(&self.input[..len], |_, _| {}, &mut self.output)
            .unwrap();
        assert_eq!(event, Event::Nothing);
        assert_eq!(written, HEADER_LEN);
        assert!(
            FrameHeader::parse(&self.output[..written])
                .unwrap()
                .has(flags::ACK)
        );
        let len =
            frame::write_frame(FrameType::Settings, flags::ACK, 0, &[], &mut self.input).unwrap();
        self.connection
            .recv(&self.input[..len], |_, _| {}, &mut self.output)
            .unwrap();
    }

    fn open(&mut self, index: usize) {
        self.sink.fill(0x5a);
        let (stream, written) = self
            .connection
            .request(
                "POST",
                "/bounded",
                "bench.test",
                "https",
                &[],
                &self.sink[..CHUNK],
                &mut self.output,
            )
            .unwrap();
        let request = FrameHeader::parse(&self.output[..written]).unwrap();
        assert_eq!(request.kind, FrameType::Headers);
        assert_eq!(request.stream, stream);
        let data_start = HEADER_LEN + request.length;
        let body = FrameHeader::parse(&self.output[data_start..written]).unwrap();
        assert_eq!(body.kind, FrameType::Data);
        assert!(body.has(flags::END_STREAM));
        assert_eq!(
            &self.output[data_start + HEADER_LEN..written],
            &self.sink[..CHUNK]
        );
        self.peer.streams[index] = stream;
        self.peer.stream_credit[index] = RECEIVE_WINDOW as usize;
        self.peer.received[index] = 0;
        self.peer.credited[index] = 0;

        // Literal insertion followed by index 62 on subsequent responses proves
        // dynamic-table persistence, including across reused stream slots.
        let mut block = [0; 128];
        block[0] = 0x88;
        let mut len = encode_header("content-length", "4194304", &mut block, 1).unwrap();
        if self.peer.indexed {
            block[len] = 0xbe;
            len += 1;
        } else {
            let entry = b"\x40\x07x-bench\x07bounded";
            block[len..len + entry.len()].copy_from_slice(entry);
            len += entry.len();
            self.peer.indexed = true;
        }
        // Split in the middle of a field, forcing genuine reassembly.
        let split = 3;
        let written = frame::write_frame(
            FrameType::Headers,
            0,
            stream,
            &block[..split],
            &mut self.input,
        )
        .unwrap();
        let (event, _) = self
            .connection
            .recv(
                &self.input[..written],
                |_, _| panic!("early header"),
                &mut self.output,
            )
            .unwrap();
        assert_eq!(event, Event::Nothing);
        let written = frame::write_frame(
            FrameType::Continuation,
            flags::END_HEADERS,
            stream,
            &block[split..len],
            &mut self.input,
        )
        .unwrap();
        let mut fields = 0;
        let (event, _) = self
            .connection
            .recv(
                &self.input[..written],
                |name, value| {
                    match name {
                        ":status" => assert_eq!(value, "200"),
                        "content-length" => assert_eq!(value, "4194304"),
                        "x-bench" => assert_eq!(value, "bounded"),
                        _ => panic!("unexpected field"),
                    }
                    fields += 1;
                },
                &mut self.output,
            )
            .unwrap();
        assert_eq!(fields, 3);
        assert_eq!(
            event,
            Event::Headers {
                stream,
                end_stream: false
            }
        );
    }

    fn transfer<const INCREMENTAL: bool>(&mut self, index: usize, offset: usize) {
        let stream = self.peer.streams[index];
        let header = FrameHeader {
            length: DEFAULT_MAX_FRAME,
            kind: FrameType::Data,
            flags: if offset + DEFAULT_MAX_FRAME == BODY {
                flags::END_STREAM
            } else {
                0
            },
            stream,
        };
        self.peer.consume(index, header.length);
        let written = if INCREMENTAL {
            // A transport adapter retains only the prefix and a 512-byte chunk.
            let mut prefix = [0; HEADER_LEN];
            header.write(&mut prefix).unwrap();
            let header = FrameHeader::parse(black_box(&prefix)).unwrap();
            for position in (0..header.length).step_by(CHUNK) {
                for (i, byte) in self.input[..CHUNK].iter_mut().enumerate() {
                    *byte = pattern(stream, offset + position + i);
                }
                self.sink[..CHUNK].copy_from_slice(black_box(&self.input[..CHUNK]));
                for (i, byte) in black_box(&self.sink[..CHUNK]).iter().enumerate() {
                    assert_eq!(*byte, pattern(stream, offset + position + i));
                }
                self.peer.received[index] += CHUNK;
            }
            self.connection
                .finish_data(header, &mut self.output)
                .unwrap()
        } else {
            header.write(&mut self.input).unwrap();
            for (i, byte) in self.input[HEADER_LEN..FRAME_BUFFER].iter_mut().enumerate() {
                *byte = pattern(stream, offset + i);
            }
            let (event, written) = self
                .connection
                .recv(
                    black_box(&self.input[..FRAME_BUFFER]),
                    |_, _| {},
                    &mut self.output,
                )
                .unwrap();
            let Event::Data {
                stream: id,
                data,
                end_stream,
            } = event
            else {
                panic!("expected DATA");
            };
            assert_eq!(id, stream);
            assert_eq!(end_stream, header.has(flags::END_STREAM));
            assert_eq!(data.as_ptr(), self.input[HEADER_LEN..].as_ptr());
            self.sink[..data.len()].copy_from_slice(data);
            for (i, byte) in black_box(&self.sink[..data.len()]).iter().enumerate() {
                assert_eq!(*byte, pattern(stream, offset + i));
            }
            self.peer.received[index] += data.len();
            written
        };
        self.peer.replies(&self.output[..written]);
    }

    fn run<const INCREMENTAL: bool>(&mut self, waves: usize) -> usize {
        self.start();
        for _ in 0..waves {
            for index in 0..MAX_STREAMS {
                self.open(index);
            }
            assert_eq!(self.connection.open_streams(), MAX_STREAMS);
            for offset in (0..BODY).step_by(DEFAULT_MAX_FRAME) {
                for index in 0..MAX_STREAMS {
                    self.transfer::<INCREMENTAL>(index, offset);
                }
            }
            assert_eq!(self.connection.open_streams(), 0);
            assert_eq!(self.peer.received, [BODY; MAX_STREAMS]);
            assert_eq!(self.peer.credited, [BODY; MAX_STREAMS]);
        }
        assert_eq!(self.peer.connection_credited, waves * MAX_STREAMS * BODY);
        assert_eq!(self.peer.connection_credit, RECEIVE_WINDOW as usize);
        self.peer.updates
    }
}

fn benchmark<const RX: usize, const SINK: usize, const INCREMENTAL: bool>(waves: usize) {
    let started = Instant::now();
    let budget = allocator::Budget::start(BUDGET);
    // This is the one caller-owned allocation. The allocator refuses growth
    // beyond the budget, including accidental allocations inside callbacks.
    let mut workspace = black_box(Box::new(Workspace::<RX, SINK>::new()));
    let updates = workspace.run::<INCREMENTAL>(waves);
    drop(workspace);
    let usage = budget.finish();
    let elapsed = started.elapsed();
    assert!(usage.peak <= BUDGET);
    assert_eq!(usage.peak, size_of::<Workspace<RX, SINK>>());
    assert_eq!(usage.allocations, 1, "protocol work allocated");
    let bytes = waves * MAX_STREAMS * BODY;
    println!(
        "{}: {bytes} B verified in {:.3} s ({:.2} MB/s)",
        if INCREMENTAL {
            "512 B incremental adapter"
        } else {
            "16 KiB complete-frame adapter"
        },
        elapsed.as_secs_f64(),
        bytes as f64 / elapsed.as_secs_f64() / 1e6
    );
    println!(
        "  peak heap: {} / {BUDGET} B; headroom: {} B; allocations: {} setup, 0 protocol",
        usage.peak,
        BUDGET - usage.peak,
        usage.allocations
    );
    println!("  {waves} waves x {MAX_STREAMS} streams; {updates} WINDOW_UPDATE frames checked");
}

fn main() {
    let quick = std::env::args().any(|arg| arg == "--quick" || arg == "--test");
    let waves = if quick { 2 } else { 8 };
    println!(
        "Strict {BUDGET} B workload heap budget; Connection: {} B",
        size_of::<Connection>()
    );
    println!("Includes client, scripted peer, frame/chunk buffers, sink and request/reply buffer.");
    println!("Host runtime, allocator metadata and stack are outside the heap budget.\n");
    allocator::verify_meter();
    benchmark::<FRAME_BUFFER, DEFAULT_MAX_FRAME, false>(waves);
    benchmark::<CHUNK, CHUNK, true>(waves);
}
