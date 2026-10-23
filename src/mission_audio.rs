use std::{
    collections::BTreeMap,
    env, fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{SyncSender, TrySendError, sync_channel},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const SAMPLE_RATE: u32 = 22_050;
const MINIMUM_CUE_INTERVAL: Duration = Duration::from_millis(90);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SoundCue {
    PowerUp,
    PowerDown,
    Ingress,
    Routed,
    Loading,
    Compression,
    Streaming,
    Completed,
    Failed,
    Disconnected,
}

impl SoundCue {
    const ALL: [Self; 10] = [
        Self::PowerUp,
        Self::PowerDown,
        Self::Ingress,
        Self::Routed,
        Self::Loading,
        Self::Compression,
        Self::Streaming,
        Self::Completed,
        Self::Failed,
        Self::Disconnected,
    ];

    fn file_name(self) -> &'static str {
        match self {
            Self::PowerUp => "power-up.wav",
            Self::PowerDown => "power-down.wav",
            Self::Ingress => "ingress.wav",
            Self::Routed => "routed.wav",
            Self::Loading => "loading.wav",
            Self::Compression => "compression.wav",
            Self::Streaming => "streaming.wav",
            Self::Completed => "completed.wav",
            Self::Failed => "failed.wav",
            Self::Disconnected => "disconnected.wav",
        }
    }

    fn score(self) -> Vec<Tone> {
        match self {
            Self::PowerUp => vec![
                Tone::square(196.0, 45),
                Tone::square(293.7, 45),
                Tone::square(392.0, 55),
                Tone::square(587.3, 90),
            ],
            Self::PowerDown => vec![
                Tone::square(587.3, 45),
                Tone::square(392.0, 45),
                Tone::square(293.7, 55),
                Tone::square(146.8, 90),
            ],
            Self::Ingress => vec![
                Tone::square(220.0, 35),
                Tone::square(330.0, 35),
                Tone::square(440.0, 55),
            ],
            Self::Routed => vec![Tone::square(659.3, 35), Tone::square(987.8, 65)],
            Self::Loading => vec![
                Tone::square(110.0, 45),
                Tone::square(146.8, 45),
                Tone::square(196.0, 70),
            ],
            Self::Compression => vec![
                Tone::square(523.3, 35),
                Tone::square(392.0, 35),
                Tone::square(659.3, 40),
                Tone::square(784.0, 75),
            ],
            Self::Streaming => vec![
                Tone::square(784.0, 30),
                Tone::silence(12),
                Tone::square(1174.7, 55),
            ],
            Self::Completed => vec![
                Tone::square(523.3, 40),
                Tone::square(659.3, 40),
                Tone::square(784.0, 85),
            ],
            Self::Failed => vec![
                Tone::noise(55),
                Tone::square(174.6, 70),
                Tone::square(116.5, 100),
                Tone::square(73.4, 150),
            ],
            Self::Disconnected => vec![
                Tone::square(293.7, 65),
                Tone::silence(25),
                Tone::square(146.8, 120),
            ],
        }
    }
}

#[derive(Clone, Copy)]
struct Tone {
    frequency: f32,
    milliseconds: u32,
    noise: bool,
}

impl Tone {
    const fn square(frequency: f32, milliseconds: u32) -> Self {
        Self {
            frequency,
            milliseconds,
            noise: false,
        }
    }

    const fn noise(milliseconds: u32) -> Self {
        Self {
            frequency: 0.0,
            milliseconds,
            noise: true,
        }
    }

    const fn silence(milliseconds: u32) -> Self {
        Self {
            frequency: 0.0,
            milliseconds,
            noise: false,
        }
    }
}

#[derive(Clone)]
struct AudioBackend {
    program: PathBuf,
    quiet_argument: Option<&'static str>,
    label: &'static str,
}

impl AudioBackend {
    fn discover() -> Option<Self> {
        [
            ("afplay", None, "afplay"),
            ("pw-play", None, "PipeWire"),
            ("paplay", None, "PulseAudio"),
            ("aplay", Some("-q"), "ALSA"),
        ]
        .into_iter()
        .find_map(|(program, quiet_argument, label)| {
            find_command(program).map(|program| Self {
                program,
                quiet_argument,
                label,
            })
        })
    }

    fn play(&self, path: &Path) {
        let mut command = Command::new(&self.program);
        if let Some(argument) = self.quiet_argument {
            command.arg(argument);
        }
        let _ = command
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

pub struct MissionAudio {
    sender: Option<SyncSender<SoundCue>>,
    worker: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    directory: Option<PathBuf>,
    backend_label: Option<&'static str>,
    last_cue: BTreeMap<SoundCue, Instant>,
}

impl MissionAudio {
    pub fn new() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let Some(backend) = AudioBackend::discover() else {
            return Self::unavailable(stop);
        };
        let directory = env::temp_dir().join(format!("infernaut-audio-{}", std::process::id()));
        if prepare_assets(&directory).is_err() {
            return Self::unavailable(stop);
        }

        let (sender, receiver) = sync_channel::<SoundCue>(12);
        let worker_directory = directory.clone();
        let worker_stop = stop.clone();
        let backend_label = backend.label;
        let worker = thread::Builder::new()
            .name("infernaut-audio".to_owned())
            .spawn(move || {
                while let Ok(cue) = receiver.recv() {
                    if worker_stop.load(Ordering::Relaxed) {
                        break;
                    }
                    backend.play(&worker_directory.join(cue.file_name()));
                }
            })
            .ok();

        let Some(worker) = worker else {
            let _ = fs::remove_dir_all(&directory);
            return Self::unavailable(stop);
        };

        Self {
            sender: Some(sender),
            worker: Some(worker),
            stop,
            directory: Some(directory),
            backend_label: Some(backend_label),
            last_cue: BTreeMap::new(),
        }
    }

    fn unavailable(stop: Arc<AtomicBool>) -> Self {
        Self {
            sender: None,
            worker: None,
            stop,
            directory: None,
            backend_label: None,
            last_cue: BTreeMap::new(),
        }
    }

    pub fn available(&self) -> bool {
        self.sender.is_some()
    }

    pub fn backend_label(&self) -> Option<&'static str> {
        self.backend_label
    }

    pub fn play(&mut self, cue: SoundCue) {
        let now = Instant::now();
        if self
            .last_cue
            .get(&cue)
            .is_some_and(|last| now.duration_since(*last) < MINIMUM_CUE_INTERVAL)
        {
            return;
        }
        let Some(sender) = &self.sender else {
            return;
        };
        match sender.try_send(cue) {
            Ok(()) => {
                self.last_cue.insert(cue, now);
            }
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {}
        }
    }
}

impl Drop for MissionAudio {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if let Some(directory) = &self.directory {
            let _ = fs::remove_dir_all(directory);
        }
    }
}

fn find_command(name: &str) -> Option<PathBuf> {
    env::var_os("PATH")
        .into_iter()
        .flat_map(|path| env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

fn prepare_assets(directory: &Path) -> io::Result<()> {
    fs::create_dir_all(directory)?;
    for cue in SoundCue::ALL {
        fs::write(
            directory.join(cue.file_name()),
            synthesize_wav(&cue.score()),
        )?;
    }
    Ok(())
}

fn synthesize_wav(score: &[Tone]) -> Vec<u8> {
    let mut samples = Vec::new();
    let mut lfsr = 0xACE1_u16;
    for tone in score {
        let count = (u64::from(SAMPLE_RATE) * u64::from(tone.milliseconds) / 1000) as usize;
        for index in 0..count {
            let edge = (count / 12).max(1);
            let envelope = if index < edge {
                index as f32 / edge as f32
            } else if index + edge > count {
                count.saturating_sub(index) as f32 / edge as f32
            } else {
                1.0
            };
            let value = if tone.noise {
                let bit = (lfsr ^ (lfsr >> 2) ^ (lfsr >> 3) ^ (lfsr >> 5)) & 1;
                lfsr = (lfsr >> 1) | (bit << 15);
                if lfsr & 1 == 0 { -1.0 } else { 1.0 }
            } else if tone.frequency > 0.0 {
                let phase = index as f32 * tone.frequency / SAMPLE_RATE as f32;
                if phase.fract() < 0.5 { 1.0 } else { -1.0 }
            } else {
                0.0
            };
            samples.push((value * envelope * 0.14 * i16::MAX as f32) as i16);
        }
    }

    let data_bytes = (samples.len() * 2) as u32;
    let mut wav = Vec::with_capacity(44 + data_bytes as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    wav.extend_from_slice(&2_u16.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_bytes.to_le_bytes());
    for sample in samples {
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    wav
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_cue_produces_a_valid_nonempty_pcm_wave() {
        for cue in SoundCue::ALL {
            let wav = synthesize_wav(&cue.score());
            assert_eq!(&wav[0..4], b"RIFF");
            assert_eq!(&wav[8..12], b"WAVE");
            assert_eq!(&wav[36..40], b"data");
            assert!(wav.len() > 44);
            assert_eq!(
                u32::from_le_bytes(wav[4..8].try_into().unwrap()) as usize + 8,
                wav.len()
            );
        }
    }
}
