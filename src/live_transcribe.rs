//! Transcribing while recording: Apple Speech (`"experiments": {"liveTranscribe": true}`) or
//! OpenAI's GPT Live Transcribe (`"transcriber": "openai"` with a saved `openaiApiKey`).
//!
//! A long-lived `scrawl-helper transcribe-stream` (Apple) or `openai-stream` (OpenAI) gets the
//! microphone's 16 kHz samples, quantized exactly as `audio.wav` will store them, about every
//! 160 ms, and keeps the finalized results. At finish the samples it hasn't had are sent from the
//! finished WAV and only that tail is left to transcribe, so the words and times are the WAV's
//! own. Apple times every word; OpenAI times each committed stretch of speech, and its words are
//! spread over it by length (`estimated`), as Hex's are. Anything wrong (the helper fails, the
//! model isn't installed, the key is refused, it lost samples or took too long) returns an error
//! and the caller transcribes the WAV as before.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::Deserialize;

use crate::model::{TranscriptData, TranscriptSegment, WordTiming};

const FEED_EVERY: Duration = Duration::from_millis(160);
/// How long finishing waits for the tail before giving up on the stream. The analyzer keeps up
/// with speech in real time, so the tail normally takes well under 100 ms; a model still loading
/// when a short recording ends takes longer, as it would after the recording too.
const TAIL_TIMEOUT: Duration = Duration::from_secs(6);

/// How long finishing waits for OpenAI's last stretch: a network round trip and the model.
const OPENAI_TAIL_TIMEOUT: Duration = Duration::from_secs(30);
pub const OPENAI_MODEL: &str = "gpt-live-transcribe";

/// Whether recordings should start a live Apple transcription now. It streams to
/// `scrawl-helper`'s SpeechAnalyzer, so only on macOS (Windows transcribes after the recording).
pub fn wanted() -> bool {
    cfg!(target_os = "macos")
        && crate::handoff::experiment(crate::handoff::LIVE_TRANSCRIBE)
        && crate::transcribe::Engine::preferred() == crate::transcribe::Engine::Apple
}

/// Whether recordings should stream to OpenAI now: it's the transcriber and a key is saved.
pub fn openai_wanted() -> bool {
    crate::transcribe::Engine::preferred() == crate::transcribe::Engine::OpenAi
        && openai_configured()
}

/// Whether OpenAI can be asked at all (macOS, through the helper, with a saved key).
pub fn openai_configured() -> bool {
    cfg!(target_os = "macos") && crate::handoff::openai_api_key().is_some()
}

/// Which recognizer a stream feeds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Engine {
    Apple,
    OpenAi,
}

pub struct Live {
    stop: Arc<AtomicBool>,
    feeder: Option<JoinHandle<Result<Feeding>>>,
    engine: Engine,
}

struct Feeding {
    child: Child,
    stdin: Option<ChildStdin>,
    fed: usize,
    lines: Receiver<Line>,
}

impl Drop for Feeding {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Event {
    Done {
        #[allow(dead_code)]
        done: bool,
        samples: usize,
    },
    Ready {
        #[allow(dead_code)]
        ready: bool,
        /// Apple's; OpenAI has none.
        #[serde(default)]
        locale: Option<String>,
    },
    Result {
        segment: TranscriptSegment,
        #[serde(default)]
        words: Vec<WordTiming>,
    },
}

enum Line {
    Event(Event),
    /// stdout closed; the helper's stderr.
    Closed(String),
}

/// What finishing took, for the log.
#[derive(Debug)]
pub struct Finished {
    pub transcript: TranscriptData,
    pub tail_ms: u64,
    pub streamed_samples: usize,
}

impl Live {
    /// Starts streaming `samples` (the recorder's 16 kHz buffer, which only grows until the
    /// recording stops) to a new helper. Nothing here blocks: the helper starts on the feeder
    /// thread.
    pub fn start(samples: Arc<Mutex<Vec<f32>>>) -> Result<Self> {
        let mut command = Command::new(crate::helper::path()?);
        command.arg("transcribe-stream");
        command.args(crate::speech_model::locale_args());
        Self::start_with(command, samples, Engine::Apple)
    }

    /// The same stream to OpenAI (`scrawl-helper openai-stream`). The key goes in the helper's
    /// environment, never its arguments.
    pub fn start_openai(samples: Arc<Mutex<Vec<f32>>>) -> Result<Self> {
        let key = crate::handoff::openai_api_key().context("No OpenAI API key is saved")?;
        let mut command = Command::new(crate::helper::path()?);
        command.arg("openai-stream");
        command.args(["--language", &crate::handoff::openai_language()]);
        command.env("SCRAWL_OPENAI_API_KEY", key);
        Self::start_with(command, samples, Engine::OpenAi)
    }

    /// Whether this stream goes to OpenAI rather than Apple Speech.
    pub fn is_openai(&self) -> bool {
        self.engine == Engine::OpenAi
    }

    fn start_with(
        mut command: Command,
        samples: Arc<Mutex<Vec<f32>>>,
        engine: Engine,
    ) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let feeder_stop = Arc::clone(&stop);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let feeder = std::thread::Builder::new()
            .name("scrawl-live-transcribe".into())
            .spawn(move || feed(command, &samples, &feeder_stop))
            .context("Could not spawn the live transcription thread")?;
        Ok(Self {
            stop,
            feeder: Some(feeder),
            engine,
        })
    }

    /// Sends what the helper hasn't had of `wav_bytes` (the recording's `audio.wav`), ends its
    /// input and collects the transcript.
    pub fn finish(mut self, wav_bytes: &[u8]) -> Result<Finished> {
        let timeout = match self.engine {
            Engine::Apple => TAIL_TIMEOUT,
            // Audio the stream hasn't had yet (a WAV transcribed after the fact) takes its own
            // time to go up and come back.
            Engine::OpenAi => OPENAI_TAIL_TIMEOUT + Duration::from_millis(wav_bytes.len() as u64 / 64),
        };
        self.finish_within(wav_bytes, timeout)
    }

    fn finish_within(&mut self, wav_bytes: &[u8], timeout: Duration) -> Result<Finished> {
        let finishing = Instant::now();
        let mut feeding = self.join_feeder()?;
        let samples = crate::audio::decode_wav_pcm16(wav_bytes)?;
        ensure!(
            feeding.fed <= samples.len(),
            "streamed {} samples but the recording has {}",
            feeding.fed,
            samples.len()
        );
        let streamed = feeding.fed;
        {
            let mut stdin = feeding.stdin.take().context("No helper stdin")?;
            stdin
                .write_all(&le_bytes(&samples[feeding.fed..]))
                .context("The live transcriber stopped")?;
        }
        let deadline = finishing + timeout;
        let mut locale = None;
        let mut segments = Vec::new();
        let mut words = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match feeding.lines.recv_timeout(left) {
                Ok(Line::Event(Event::Ready { locale: l, .. })) => locale = Some(l),
                Ok(Line::Event(Event::Result {
                    segment,
                    words: more,
                })) => {
                    segments.push(segment);
                    words.extend(more);
                }
                Ok(Line::Event(Event::Done { samples: heard, .. })) => {
                    ensure!(
                        heard == samples.len(),
                        "the live transcriber heard {heard} samples of {}",
                        samples.len()
                    );
                    break;
                }
                Ok(Line::Closed(stderr)) => bail!(
                    "the live transcriber stopped: {}",
                    stderr.trim().trim_start_matches("scrawl-helper: ")
                ),
                Err(RecvTimeoutError::Timeout) => {
                    bail!(
                        "the live transcriber took over {} s to finish",
                        timeout.as_secs()
                    )
                }
                Err(RecvTimeoutError::Disconnected) => bail!("the live transcriber vanished"),
            }
        }
        let transcript = match self.engine {
            Engine::Apple => {
                let locale = locale
                    .flatten()
                    .context("the live transcriber never said it was ready")?;
                transcript(&locale, segments, words)
            }
            Engine::OpenAi => openai_transcript(segments),
        };
        Ok(Finished {
            transcript,
            tail_ms: finishing.elapsed().as_millis() as u64,
            streamed_samples: streamed,
        })
    }

    fn join_feeder(&mut self) -> Result<Feeding> {
        self.stop.store(true, Ordering::Release);
        let feeder = self.feeder.take().context("Already finished")?;
        feeder.thread().unpark();
        feeder
            .join()
            .map_err(|_| anyhow!("The live transcription thread panicked"))?
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        if self.feeder.is_some() {
            let _ = self.join_feeder();
        }
    }
}

/// As `helper::transcribe` builds it from the batch helper's output.
fn transcript(
    locale: &str,
    mut segments: Vec<TranscriptSegment>,
    mut words: Vec<WordTiming>,
) -> TranscriptData {
    segments.sort_by_key(|s| s.t0);
    words.sort_by_key(|w| w.t0);
    TranscriptData {
        source: "speech-analyzer".into(),
        model: Some(format!("SpeechTranscriber {locale}")),
        text: segments
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join(" "),
        segments,
        words,
        estimated: false,
        missing: None,
    }
}

/// OpenAI's stretches of speech, their words spread over each by length, as Hex's are.
fn openai_transcript(mut segments: Vec<TranscriptSegment>) -> TranscriptData {
    segments.sort_by_key(|s| s.t0);
    let mut transcript = TranscriptData {
        source: format!("openai:{OPENAI_MODEL}"),
        model: Some(OPENAI_MODEL.to_owned()),
        estimated: true,
        ..Default::default()
    };
    let mut texts = Vec::new();
    for segment in segments {
        let (spread, words) = crate::hex::spread_words(&segment.text, segment.t0, segment.t1);
        transcript.segments.extend(spread);
        transcript.words.extend(words);
        texts.push(segment.text);
    }
    transcript.text = texts.join(" ");
    transcript
}

/// Transcribes a finished recording with OpenAI: the whole WAV goes through a fresh stream.
pub fn transcribe_wav_openai(wav_bytes: &[u8]) -> Result<TranscriptData> {
    let live = Live::start_openai(Arc::new(Mutex::new(Vec::new())))?;
    Ok(live.finish(wav_bytes)?.transcript)
}

fn feed(mut command: Command, samples: &Mutex<Vec<f32>>, stop: &AtomicBool) -> Result<Feeding> {
    let mut child = command
        .spawn()
        .context("Could not start scrawl-helper's transcription stream")?;
    let stdin = child.stdin.take().context("No helper stdin")?;
    let stdout = child.stdout.take().context("No helper stdout")?;
    let mut stderr = child.stderr.take().context("No helper stderr")?;
    let (sender, lines) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("scrawl-live-transcribe-out".into())
        .spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                match serde_json::from_str::<Event>(&line) {
                    Ok(event) => {
                        let _ = sender.send(Line::Event(event));
                    }
                    Err(error) => crate::capture::scrawl_log(format!(
                        "Live transcription: unreadable line ({error})"
                    )),
                }
            }
            let mut message = String::new();
            let _ = stderr.read_to_string(&mut message);
            let _ = sender.send(Line::Closed(message));
        })
        .context("Could not spawn the live transcription reader")?;
    let mut feeding = Feeding {
        child,
        stdin: Some(stdin),
        fed: 0,
        lines,
    };
    let mut bytes = Vec::new();
    while !stop.load(Ordering::Acquire) {
        bytes.clear();
        {
            let buffer = samples
                .lock()
                .map_err(|_| anyhow!("Microphone buffer lock poisoned"))?;
            // The recorder empties the buffer when it stops; the rest comes from the WAV.
            if let Some(fresh) = buffer.get(feeding.fed..) {
                bytes.extend(
                    fresh
                        .iter()
                        .flat_map(|&s| crate::audio::pcm16(s).to_le_bytes()),
                );
            }
        }
        if !bytes.is_empty() {
            let stdin = feeding.stdin.as_mut().context("No helper stdin")?;
            stdin
                .write_all(&bytes)
                .context("The live transcriber stopped")?;
            feeding.fed += bytes.len() / 2;
        }
        std::thread::park_timeout(FEED_EVERY);
    }
    Ok(feeding)
}

fn le_bytes(samples: &[i16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

/// Plays `wav_bytes` into a live transcription as a microphone would, `speed` times real time in
/// 10 ms callbacks, and returns it ready to finish (`scrawl reprocess --bench N --live-speed S`).
pub fn simulate(wav_bytes: &[u8], speed: f64) -> Result<Live> {
    let samples = crate::audio::decode_wav_16k(wav_bytes)?;
    let buffer = Arc::new(Mutex::new(Vec::with_capacity(samples.len())));
    let live = Live::start(Arc::clone(&buffer))?;
    let started = Instant::now();
    for (i, chunk) in samples.chunks(160).enumerate() {
        buffer
            .lock()
            .map_err(|_| anyhow!("poisoned"))?
            .extend_from_slice(chunk);
        let due = Duration::from_secs_f64((i + 1) as f64 * 0.01 / speed.max(0.01));
        if let Some(wait) = due.checked_sub(started.elapsed()) {
            std::thread::sleep(wait);
        }
    }
    // The recorder empties its buffer when it stops.
    buffer.lock().map_err(|_| anyhow!("poisoned"))?.clear();
    Ok(live)
}

/// Shares a live transcription between copies of a recording; the first `take` gets it.
#[derive(Clone, Default)]
pub struct Slot(Arc<Mutex<Option<Live>>>);

impl Slot {
    pub fn new(live: Option<Live>) -> Self {
        Self(Arc::new(Mutex::new(live)))
    }

    pub fn take(&self) -> Option<Live> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

impl std::fmt::Debug for Slot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let live = self.0.lock().map(|live| live.is_some()).unwrap_or(false);
        f.debug_tuple("Slot").field(&live).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in helper: echoes how many samples it read, as the real one does, after the
    /// given lines.
    fn fake(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        command
    }

    #[cfg(unix)]
    fn wav(samples: &[f32]) -> Vec<u8> {
        crate::audio::encode_wav_16k(samples).unwrap()
    }

    #[cfg(unix)]
    const READY: &str = r#"echo '{"ready":true,"locale":"en_US"}'"#;

    #[cfg(unix)]
    #[test]
    fn collects_finalized_results_and_counts_every_sample() {
        let script = format!(
            r#"{READY}; n=$(wc -c | tr -d ' '); echo '{{"segment":{{"t0":900,"t1":1500,"text":"Look here."}},"words":[{{"t0":900,"t1":1200,"text":"Look"}},{{"t0":1200,"t1":1500,"text":"here."}}]}}'; echo '{{"segment":{{"t0":60,"t1":600,"text":"Okay,"}},"words":[{{"t0":60,"t1":600,"text":"Okay,"}}]}}'; echo "{{\"done\":true,\"samples\":$((n / 2))}}""#
        );
        let buffer = Arc::new(Mutex::new(vec![0.25_f32; 4_000]));
        let mut live = Live::start_with(fake(&script), Arc::clone(&buffer), Engine::Apple).unwrap();
        std::thread::sleep(Duration::from_millis(250));
        let mut all = vec![0.25_f32; 4_000];
        all.extend([-0.5_f32; 1_000]);
        let done = live
            .finish_within(&wav(&all), Duration::from_secs(5))
            .unwrap();
        assert_eq!(done.streamed_samples, 4_000);
        assert_eq!(done.transcript.text, "Okay, Look here.");
        assert_eq!(done.transcript.source, "speech-analyzer");
        assert_eq!(
            done.transcript.model.as_deref(),
            Some("SpeechTranscriber en_US")
        );
        assert_eq!(
            done.transcript
                .words
                .iter()
                .map(|w| (w.t0, w.text.as_str()))
                .collect::<Vec<_>>(),
            vec![(60, "Okay,"), (900, "Look"), (1200, "here.")]
        );
    }

    #[cfg(unix)]
    #[test]
    fn streams_exactly_the_samples_the_wav_holds() {
        let out = std::env::temp_dir().join(format!("scrawl-live-pcm-{}", std::process::id()));
        let script = format!(
            r#"{READY}; cat > '{}'; echo '{{"done":true,"samples":3}}'"#,
            out.display()
        );
        let buffer = Arc::new(Mutex::new(vec![0.5_f32, -0.25]));
        let mut live = Live::start_with(fake(&script), Arc::clone(&buffer), Engine::Apple).unwrap();
        std::thread::sleep(Duration::from_millis(250));
        buffer.lock().unwrap().clear();
        let recording = [0.5_f32, -0.25, 1.5];
        live.finish_within(&wav(&recording), Duration::from_secs(5))
            .unwrap();
        let written = std::fs::read(&out).unwrap();
        let _ = std::fs::remove_file(&out);
        let expected = crate::audio::decode_wav_pcm16(&wav(&recording)).unwrap();
        assert_eq!(written, le_bytes(&expected));
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_helper_or_lost_samples_is_an_error() {
        let buffer = || Arc::new(Mutex::new(vec![0.1_f32; 320]));
        let failed = Live::start_with(
            fake("echo 'scrawl-helper: the speech model isnt installed' >&2; exit 1"),
            buffer(),
            Engine::Apple,
        )
        .unwrap()
        .finish_within(&wav(&[0.1; 320]), Duration::from_secs(5));
        assert!(failed.is_err());

        let short = format!(r#"{READY}; cat > /dev/null; echo '{{"done":true,"samples":7}}'"#);
        let lost = Live::start_with(fake(&short), buffer(), Engine::Apple)
            .unwrap()
            .finish_within(&wav(&[0.1; 320]), Duration::from_secs(5));
        assert!(lost.unwrap_err().to_string().contains("heard 7 samples"));

        let hung = Live::start_with(
            fake(&format!("{READY}; cat > /dev/null; sleep 30")),
            buffer(),
            Engine::Apple,
        )
        .unwrap()
        .finish_within(&wav(&[0.1; 320]), Duration::from_millis(300));
        assert!(hung.unwrap_err().to_string().contains("to finish"));
    }

    #[cfg(unix)]
    #[test]
    fn openai_stretches_are_spread_into_estimated_words() {
        let script = r#"echo '{"ready":true}'; n=$(wc -c | tr -d ' '); echo '{"segment":{"t0":2000,"t1":3000,"text":"Look here now."}}'; echo '{"segment":{"t0":0,"t1":1000,"text":"Okay, so"}}'; echo "{\"done\":true,\"samples\":$((n / 2))}""#;
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let mut live = Live::start_with(fake(script), buffer, Engine::OpenAi).unwrap();
        let done = live
            .finish_within(&wav(&[0.1; 48_000]), Duration::from_secs(5))
            .unwrap();
        assert_eq!(done.transcript.source, "openai:gpt-live-transcribe");
        assert!(done.transcript.estimated);
        assert_eq!(done.transcript.text, "Okay, so Look here now.");
        let words: Vec<_> = done
            .transcript
            .words
            .iter()
            .map(|w| (w.t0, w.t1, w.text.as_str()))
            .collect();
        assert_eq!(words[0], (0, 714, "Okay,"));
        assert_eq!(words[1], (714, 1000, "so"));
        assert_eq!(words[2].2, "Look");
        assert_eq!(words[4], (2_666, 3_000, "now."));
        assert_eq!(done.transcript.segments.len(), 2);
    }

    #[test]
    fn dropping_an_unfinished_stream_stops_the_helper() {
        let live =
            Live::start_with(fake("sleep 30"), Arc::new(Mutex::new(Vec::new())), Engine::Apple)
                .unwrap();
        let started = Instant::now();
        drop(live);
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
