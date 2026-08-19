// SPDX-License-Identifier: Apache-2.0
//! Original generated-frame producer used to prove process and transport
//! boundaries before any Wallpaper Engine parser or scene renderer is added.

use std::{
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use clap::Parser;
use kwe_frame_protocol::{FrameSpec, ProducerState, SharedFrameWriter};
use kwe_input_protocol::{
    InputAck, MAX_MESSAGE_BYTES as MAX_INPUT_MESSAGE_BYTES, PointerPhase, decode_pointer_line,
    encode_ack_line,
};

#[derive(Debug, Parser)]
#[command(version, about = "Isolated KWE generated test-pattern renderer")]
struct Arguments {
    #[arg(long)]
    output: PathBuf,
    /// Stall before creating the frame mapping (supervisor startup test).
    #[arg(long)]
    startup_hang: bool,
    #[arg(long, default_value_t = 960, value_parser = clap::value_parser!(u32).range(1..=8192))]
    width: u32,
    #[arg(long, default_value_t = 540, value_parser = clap::value_parser!(u32).range(1..=8192))]
    height: u32,
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u32).range(1..=240))]
    fps: u32,
    /// Stop cleanly after this many published frames; unlimited when omitted.
    #[arg(long)]
    frames: Option<u64>,
    /// Stop publishing but remain alive after this sequence number.
    #[arg(long)]
    hang_after: Option<u64>,
    /// Corrupt the protocol magic after this sequence number and remain alive.
    #[arg(long)]
    corrupt_after: Option<u64>,
    /// Exit abruptly with code 70 after this sequence number.
    #[arg(long)]
    exit_after: Option<u64>,
    /// Ignore SIGTERM to exercise the supervisor's bounded SIGKILL fallback.
    #[arg(long)]
    ignore_term: bool,
    /// Attempt a bounded virtual allocation after this sequence number.
    #[arg(long)]
    memory_pressure_after: Option<u64>,
    /// Allocation size used with --memory-pressure-after.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=4096))]
    memory_pressure_mib: Option<u64>,
    /// Print this many diagnostic lines to stderr at startup, so the daemon's
    /// bounded stderr ring can be observed in smoke tests.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=4096))]
    stderr_lines: Option<u32>,
}

fn main() -> Result<()> {
    let arguments = Arguments::parse();
    if arguments.memory_pressure_after.is_some() != arguments.memory_pressure_mib.is_some() {
        anyhow::bail!(
            "--memory-pressure-after and --memory-pressure-mib must be supplied together"
        );
    }
    if let Some(count) = arguments.stderr_lines {
        for index in 0..count {
            eprintln!("event=renderer.stderr_line index={index}");
        }
    }
    if arguments.ignore_term {
        // SAFETY: installing SIG_IGN for SIGTERM uses a process-global constant
        // handler and is performed before this single-threaded test worker runs.
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
        }
    }
    let mut input_channel = InputChannel::new()?;
    if arguments.startup_hang {
        eprintln!("event=renderer.fault kind=startup_hang");
        park_forever();
    }
    let spec = FrameSpec::new(arguments.width, arguments.height)?;
    let mut writer = SharedFrameWriter::create(&arguments.output, spec)
        .with_context(|| format!("create frame mapping {}", arguments.output.display()))?;
    let mut pixels = vec![0_u8; spec.pixel_bytes()];
    let interval = Duration::from_secs_f64(1.0 / f64::from(arguments.fps));
    let mut deadline = Instant::now();
    let mut published = 0_u64;

    loop {
        if arguments.frames.is_some_and(|limit| published >= limit) {
            writer.set_state(ProducerState::Stopping);
            eprintln!("event=renderer.complete frames={published}");
            return Ok(());
        }
        if arguments.exit_after.is_some_and(|limit| published >= limit) {
            eprintln!("event=renderer.fault kind=exit frames={published}");
            std::process::exit(70);
        }
        if arguments
            .corrupt_after
            .is_some_and(|limit| published >= limit)
        {
            eprintln!("event=renderer.fault kind=corrupt frames={published}");
            writer.corrupt_magic_for_test();
            park_forever();
        }
        if arguments.hang_after.is_some_and(|limit| published >= limit) {
            eprintln!("event=renderer.fault kind=hang frames={published}");
            park_forever();
        }
        if arguments
            .memory_pressure_after
            .is_some_and(|limit| published >= limit)
        {
            let mib = arguments.memory_pressure_mib.unwrap_or_default();
            let bytes = usize::try_from(mib.saturating_mul(1024 * 1024))
                .context("memory pressure size does not fit usize")?;
            let mut pressure = Vec::<u8>::new();
            if pressure.try_reserve_exact(bytes).is_err() {
                eprintln!(
                    "event=renderer.fault kind=memory_pressure outcome=allocation_denied mib={mib}"
                );
                std::process::exit(71);
            }
            eprintln!(
                "event=renderer.fault kind=memory_pressure outcome=unexpected_success mib={mib}"
            );
            std::process::exit(72);
        }

        input_channel.poll();
        draw_test_pattern(
            &mut pixels,
            spec,
            published,
            input_channel.state.pointer(spec),
        );
        published = writer.publish(&pixels)?;
        deadline += interval;
        if let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            thread::sleep(remaining);
        } else {
            deadline = Instant::now();
        }
    }
}

#[derive(Debug, Default)]
struct InputState {
    sequence: u64,
    x: u16,
    y: u16,
    inside: bool,
}

impl InputState {
    fn record(&mut self, sequence: u64, phase: PointerPhase, x: u16, y: u16) {
        self.x = x;
        self.y = y;
        self.inside = phase != PointerPhase::Leave;
        self.sequence = sequence;
    }

    fn pointer(&self, spec: FrameSpec) -> Option<(usize, usize)> {
        if !self.inside || self.sequence == 0 {
            return None;
        }
        let x = u64::from(self.x) * u64::from(spec.width.saturating_sub(1)) / u64::from(u16::MAX);
        let y = u64::from(self.y) * u64::from(spec.height.saturating_sub(1)) / u64::from(u16::MAX);
        Some((x as usize, y as usize))
    }
}

struct InputChannel {
    state: InputState,
    buffer: Vec<u8>,
}

impl InputChannel {
    fn new() -> Result<Self> {
        set_nonblocking(libc::STDIN_FILENO).context("configure renderer input")?;
        set_nonblocking(libc::STDOUT_FILENO).context("configure renderer input acknowledgement")?;
        Ok(Self {
            state: InputState::default(),
            buffer: Vec::with_capacity(MAX_INPUT_MESSAGE_BYTES),
        })
    }

    fn poll(&mut self) {
        let mut total = 0_usize;
        while total < MAX_INPUT_MESSAGE_BYTES * 4 {
            let mut chunk = [0_u8; MAX_INPUT_MESSAGE_BYTES];
            // SAFETY: `chunk` is a valid writable buffer and stdin remains open
            // for the renderer lifetime.
            let read = unsafe {
                libc::read(
                    libc::STDIN_FILENO,
                    chunk.as_mut_ptr().cast::<libc::c_void>(),
                    chunk.len(),
                )
            };
            if read <= 0 {
                break;
            }
            let read = read as usize;
            total += read;
            self.buffer.extend_from_slice(&chunk[..read]);
            while let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = self.buffer.drain(..=newline).collect();
                let Ok(message) = decode_pointer_line(&line) else {
                    continue;
                };
                self.state
                    .record(message.sequence, message.phase, message.x, message.y);
                let Ok(ack) = InputAck::new(message.sequence).and_then(|ack| encode_ack_line(&ack))
                else {
                    continue;
                };
                // SAFETY: acknowledgements are bounded immutable byte slices;
                // stdout is the daemon-owned nonblocking acknowledgement pipe.
                unsafe {
                    libc::write(
                        libc::STDOUT_FILENO,
                        ack.as_ptr().cast::<libc::c_void>(),
                        ack.len(),
                    );
                }
            }
            if self.buffer.len() > MAX_INPUT_MESSAGE_BYTES {
                self.buffer.clear();
            }
        }
    }
}

fn set_nonblocking(descriptor: libc::c_int) -> std::io::Result<()> {
    // SAFETY: fcntl reads and updates flags for a valid inherited descriptor.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn park_forever() -> ! {
    loop {
        thread::park_timeout(Duration::from_secs(60));
    }
}

fn draw_test_pattern(
    pixels: &mut [u8],
    spec: FrameSpec,
    frame: u64,
    pointer: Option<(usize, usize)>,
) {
    let width = spec.width as usize;
    let height = spec.height as usize;
    let motion = (frame as usize * 7) % width;
    for y in 0..height {
        for x in 0..width {
            let offset = y * spec.stride as usize + x * 4;
            let checker = ((x / 64) + (y / 64)) % 2 == 0;
            let blue = ((x * 255) / width.max(1)) as u8;
            let green = ((y * 255) / height.max(1)) as u8;
            let mut red = if checker { 42 } else { 18 };
            if x.abs_diff(motion) < 5 {
                red = 255;
            }
            if x % 160 < 2 || y % 160 < 2 {
                red = 220;
            }
            let pointer_marker = pointer.is_some_and(|(pointer_x, pointer_y)| {
                (x.abs_diff(pointer_x) <= 5 && y.abs_diff(pointer_y) <= 1)
                    || (x.abs_diff(pointer_x) <= 1 && y.abs_diff(pointer_y) <= 5)
            });
            pixels[offset] = blue;
            pixels[offset + 1] = if pointer_marker { 255 } else { green };
            pixels[offset + 2] = if pointer_marker { 255 } else { red };
            pixels[offset + 3] = 255;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pattern_is_opaque_and_changes_with_sequence() {
        let spec = FrameSpec::new(32, 16).unwrap();
        let mut first = vec![0; spec.pixel_bytes()];
        let mut second = first.clone();
        draw_test_pattern(&mut first, spec, 0, None);
        draw_test_pattern(&mut second, spec, 3, None);
        assert!(first.chunks_exact(4).all(|pixel| pixel[3] == 255));
        assert_ne!(first, second);
    }

    #[test]
    fn pointer_position_adds_a_visible_crosshair() {
        let spec = FrameSpec::new(32, 16).unwrap();
        let mut baseline = vec![0; spec.pixel_bytes()];
        let mut interactive = baseline.clone();
        draw_test_pattern(&mut baseline, spec, 4, None);
        draw_test_pattern(&mut interactive, spec, 4, Some((16, 8)));
        assert_ne!(baseline, interactive);
        let center = 8 * spec.stride as usize + 16 * 4;
        assert_eq!(&interactive[center..center + 4], &[127, 255, 255, 255]);
    }
}
