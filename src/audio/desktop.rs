//! Local-file output on a PC (development builds), through rodio.

use std::time::Duration;

use rodio::buffer::SamplesBuffer;
use rodio::{OutputStream, OutputStreamBuilder, Sink};

use super::{AudioOut, OutBuf, OutSpec, SampleFormat};

pub struct RodioOut {
    stream: Option<OutputStream>,
    sink: Option<Sink>,
    spec: Option<OutSpec>,
    queued_frames: Vec<usize>,
}

impl RodioOut {
    pub fn new() -> Self {
        Self {
            stream: None,
            sink: None,
            spec: None,
            queued_frames: Vec::new(),
        }
    }

    fn prune(&mut self) {
        if let Some(sink) = &self.sink {
            let len = sink.len();
            while self.queued_frames.len() > len {
                self.queued_frames.remove(0);
            }
        }
    }
}

impl AudioOut for RodioOut {
    fn open(&mut self, rate: u32) -> Result<OutSpec, String> {
        self.close();
        if self.stream.is_none() {
            let mut s = OutputStreamBuilder::open_default_stream().map_err(|e| e.to_string())?;
            s.log_on_drop(false);
            self.stream = Some(s);
        }
        let sink = Sink::connect_new(self.stream.as_ref().unwrap().mixer());
        self.sink = Some(sink);
        let spec = OutSpec {
            rate,
            format: SampleFormat::S32,
        };
        self.spec = Some(spec);
        Ok(spec)
    }

    fn write(&mut self, buf: OutBuf) -> Result<(), String> {
        let (Some(sink), Some(spec)) = (&self.sink, self.spec) else {
            return Err("output not open".into());
        };
        let data: Vec<f32> = match buf {
            OutBuf::S16(s) => s.iter().map(|&v| v as f32 / 32768.0).collect(),
            OutBuf::S32(s) => s.iter().map(|&v| v as f32 / 2_147_483_648.0).collect(),
        };
        let frames = data.len() / 2;
        sink.append(SamplesBuffer::new(2, spec.rate, data));
        self.queued_frames.push(frames);
        // Keep about four chunks queued, like a device buffer.
        loop {
            self.prune();
            if self.queued_frames.len() <= 4 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    fn delay_frames(&mut self) -> u32 {
        self.prune();
        self.queued_frames.iter().sum::<usize>() as u32
    }

    fn drain(&mut self) {
        if let Some(sink) = &self.sink {
            while !sink.empty() {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        self.close();
    }

    fn close(&mut self) {
        if let Some(sink) = self.sink.take() {
            sink.clear();
        }
        self.queued_frames.clear();
        self.spec = None;
    }
}
