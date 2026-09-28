#[cfg(not(all(windows, feature = "native-audio-spike")))]
fn main() {
    eprintln!("audio-backend-benchmark requires Windows + --features native-audio-spike");
    std::process::exit(2);
}

#[cfg(all(windows, feature = "native-audio-spike"))]
mod windows_bench {
    use aivtuber_adapters::{
        AudioPlayRequest, AudioPlayer, ProcessAudioConfig, ProcessAudioPlayer, RodioAudioPlayer,
    };
    use serde_json::{Value, json};
    use std::error::Error;
    use std::fs::{self, File};
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::thread;
    use std::time::{Duration, Instant};

    const SAMPLE_RATE: u32 = 48_000;

    pub fn run() -> Result<(), Box<dyn Error>> {
        let repetitions = std::env::var("AIVTUBER_AUDIO_BENCH_REPETITIONS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(40)
            .max(5);
        let output = std::env::args().nth(1).map(PathBuf::from);
        let root = std::env::temp_dir().join("aivtuber-audio-benchmark");
        fs::create_dir_all(&root)?;
        write_wav(&root.join("short.wav"), 120)?;
        write_wav(&root.join("long.wav"), 2_000)?;

        let process = ProcessAudioPlayer::new(ProcessAudioConfig {
            root: root.clone(),
            program: "ffplay".to_owned(),
            args: vec![
                "-nodisp".to_owned(),
                "-autoexit".to_owned(),
                "-loglevel".to_owned(),
                "error".to_owned(),
                "-volume".to_owned(),
                "0".to_owned(),
                "-af".to_owned(),
                "atempo={tempo}".to_owned(),
                "{audio}".to_owned(),
            ],
        })?;

        let native_init_started = Instant::now();
        let native = RodioAudioPlayer::new(root.clone())?;
        let native_init_us = micros(native_init_started.elapsed());

        let short = AudioPlayRequest {
            audio_ref: "audio://short.wav".to_owned(),
            speed_factor: 1.0,
        };
        let long = AudioPlayRequest {
            audio_ref: "audio://long.wav".to_owned(),
            speed_factor: 1.0,
        };
        let process_cold_start_us = one_process_start(&process, &short)?;
        let native_first_start_us = one_native_start(&native, &short)?;
        let host_before = process_snapshot(std::process::id());
        let process_start = measure_process_start(&process, &short, repetitions)?;
        let native_start = measure_native_start(&native, &short, repetitions)?;
        let (process_stop, ffplay_sample) = measure_process_stop(&process, &long, repetitions)?;
        let native_stop = measure_native_stop(&native, &long, repetitions)?;
        let process_handoff = measure_process_handoff(&process, &short, repetitions)?;
        let native_handoff = measure_native_handoff(&native, &short, repetitions)?;
        let host_after = process_snapshot(std::process::id());

        let report = json!({
            "schema": "aivtuber.audio-backend-benchmark.v1",
            "environment": {
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
                "rodio_version": "0.22.2",
                "process_program": "ffplay",
                "sample_format": "pcm_s16le_mono_48000hz_wav",
                "repetitions": repetitions,
                "measurement_boundary": "backend start/queue proxy; not acoustic first-audible latency",
                "native_cold_device_init_us": native_init_us,
                "process_cold_start_proxy_us": process_cold_start_us,
                "native_first_start_proxy_us": native_first_start_us,
            },
            "process": {
                "start_proxy_us": distribution(&process_start),
                "stop_us": distribution(&process_stop),
                "consecutive_handoff_proxy_us": distribution(&process_handoff),
            },
            "rodio": {
                "start_proxy_us": distribution(&native_start),
                "stop_us": distribution(&native_stop),
                "consecutive_handoff_proxy_us": distribution(&native_handoff),
            },
            "resource_observation": {
                "benchmark_host_before": host_before,
                "benchmark_host_after": host_after,
                "ffplay_child_sample": ffplay_sample,
                "note": "coarse OS process snapshots; host includes harness/native stream and ffplay is a separate child"
            }
        });
        let text = serde_json::to_string_pretty(&report)?;
        if let Some(path) = output {
            fs::write(path, format!("{text}\n"))?;
        } else {
            println!("{text}");
        }
        Ok(())
    }

    fn one_process_start(
        player: &ProcessAudioPlayer,
        request: &AudioPlayRequest,
    ) -> Result<u64, Box<dyn Error>> {
        let started = Instant::now();
        let mut child = player.start_tracked(request)?;
        let value = micros(started.elapsed());
        let _ = child.kill();
        let _ = child.wait();
        Ok(value)
    }

    fn one_native_start(
        player: &RodioAudioPlayer,
        request: &AudioPlayRequest,
    ) -> Result<u64, Box<dyn Error>> {
        let started = Instant::now();
        player.play(request)?;
        let value = micros(started.elapsed());
        player.stop();
        Ok(value)
    }

    fn measure_process_start(
        player: &ProcessAudioPlayer,
        request: &AudioPlayRequest,
        repetitions: usize,
    ) -> Result<Vec<u64>, Box<dyn Error>> {
        let mut values = Vec::with_capacity(repetitions);
        for _ in 0..repetitions {
            let started = Instant::now();
            let mut child = player.start_tracked(request)?;
            values.push(micros(started.elapsed()));
            let _ = child.kill();
            let _ = child.wait();
        }
        Ok(values)
    }

    fn measure_native_start(
        player: &RodioAudioPlayer,
        request: &AudioPlayRequest,
        repetitions: usize,
    ) -> Result<Vec<u64>, Box<dyn Error>> {
        let mut values = Vec::with_capacity(repetitions);
        for _ in 0..repetitions {
            let started = Instant::now();
            player.play(request)?;
            values.push(micros(started.elapsed()));
            player.stop();
        }
        Ok(values)
    }

    fn measure_process_stop(
        player: &ProcessAudioPlayer,
        request: &AudioPlayRequest,
        repetitions: usize,
    ) -> Result<(Vec<u64>, Option<Value>), Box<dyn Error>> {
        let mut values = Vec::with_capacity(repetitions);
        let mut resource_sample = None;
        for index in 0..repetitions {
            let mut child = player.start_tracked(request)?;
            thread::sleep(Duration::from_millis(10));
            if index == 0 {
                resource_sample = process_snapshot(child.id());
            }
            let started = Instant::now();
            child.kill()?;
            child.wait()?;
            values.push(micros(started.elapsed()));
        }
        Ok((values, resource_sample))
    }
    fn measure_native_stop(
        player: &RodioAudioPlayer,
        request: &AudioPlayRequest,
        repetitions: usize,
    ) -> Result<Vec<u64>, Box<dyn Error>> {
        let mut values = Vec::with_capacity(repetitions);
        for _ in 0..repetitions {
            player.play(request)?;
            thread::sleep(Duration::from_millis(10));
            let started = Instant::now();
            player.stop();
            values.push(micros(started.elapsed()));
        }
        Ok(values)
    }

    fn measure_process_handoff(
        player: &ProcessAudioPlayer,
        request: &AudioPlayRequest,
        repetitions: usize,
    ) -> Result<Vec<u64>, Box<dyn Error>> {
        let mut values = Vec::with_capacity(repetitions);
        for _ in 0..repetitions {
            let mut first = player.start_tracked(request)?;
            first.wait()?;
            let started = Instant::now();
            let mut second = player.start_tracked(request)?;
            values.push(micros(started.elapsed()));
            let _ = second.kill();
            let _ = second.wait();
        }
        Ok(values)
    }

    fn measure_native_handoff(
        player: &RodioAudioPlayer,
        request: &AudioPlayRequest,
        repetitions: usize,
    ) -> Result<Vec<u64>, Box<dyn Error>> {
        let mut values = Vec::with_capacity(repetitions);
        for _ in 0..repetitions {
            player.play(request)?;
            wait_until_idle(player)?;
            let started = Instant::now();
            player.play(request)?;
            values.push(micros(started.elapsed()));
            player.stop();
        }
        Ok(values)
    }
    fn wait_until_idle(player: &RodioAudioPlayer) -> Result<(), Box<dyn Error>> {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !player.is_idle() {
            if Instant::now() >= deadline {
                return Err("native audio did not become idle before benchmark timeout".into());
            }
            thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }

    fn process_snapshot(pid: u32) -> Option<Value> {
        let script = format!(
            "$p=Get-Process -Id {pid}; [pscustomobject]@{{working_set_bytes=$p.WorkingSet64; private_bytes=$p.PrivateMemorySize64; cpu_seconds=$p.CPU}} | ConvertTo-Json -Compress"
        );
        let output = Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", &script])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        serde_json::from_slice(&output.stdout).ok()
    }

    fn distribution(values: &[u64]) -> Value {
        let mut sorted = values.to_vec();
        sorted.sort_unstable();
        json!({
            "p50": nearest_rank(&sorted, 50),
            "p95": nearest_rank(&sorted, 95),
            "p99": nearest_rank(&sorted, 99),
            "min": sorted[0],
            "max": sorted[sorted.len() - 1],
        })
    }

    fn nearest_rank(sorted: &[u64], percentile: usize) -> u64 {
        let rank = ((percentile * sorted.len()) + 99) / 100;
        sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
    }

    fn micros(duration: Duration) -> u64 {
        duration.as_micros().min(u128::from(u64::MAX)) as u64
    }

    fn write_wav(path: &Path, duration_ms: u32) -> Result<(), Box<dyn Error>> {
        let samples = (u64::from(SAMPLE_RATE) * u64::from(duration_ms) / 1_000) as u32;
        let data_bytes = samples * 2;
        let mut file = File::create(path)?;
        file.write_all(b"RIFF")?;
        file.write_all(&(36 + data_bytes).to_le_bytes())?;
        file.write_all(b"WAVEfmt ")?;
        file.write_all(&16_u32.to_le_bytes())?;
        file.write_all(&1_u16.to_le_bytes())?;
        file.write_all(&1_u16.to_le_bytes())?;
        file.write_all(&SAMPLE_RATE.to_le_bytes())?;
        file.write_all(&(SAMPLE_RATE * 2).to_le_bytes())?;
        file.write_all(&2_u16.to_le_bytes())?;
        file.write_all(&16_u16.to_le_bytes())?;
        file.write_all(b"data")?;
        file.write_all(&data_bytes.to_le_bytes())?;
        for _ in 0..samples {
            file.write_all(&0_i16.to_le_bytes())?;
        }
        Ok(())
    }
}

#[cfg(all(windows, feature = "native-audio-spike"))]
fn main() {
    if let Err(error) = windows_bench::run() {
        eprintln!("audio-backend-benchmark: {error}");
        std::process::exit(1);
    }
}
