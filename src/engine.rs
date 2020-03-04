use crossbeam_queue::SegQueue;
use std::sync::Arc;
use crate::protos::*;
use crate::protos::command::CommandOneof;
use crate::sample::Sample;
use crate::protos::looper_command::TargetOneof;
use crate::music::*;
use crate::looper::Looper;
use crate::midi::MidiEvent;
use crate::metronome::Metronome;
use std::f32::NEG_INFINITY;

struct Trigger {

}

pub struct Engine {
    config: Config,

    time: i64,
    time_signature: TimeSignature,
    tempo: Tempo,

    gui_output: Arc<SegQueue<State>>,
    gui_input: Arc<SegQueue<Command>>,

    loopers: Vec<Looper>,
    active: u32,

    metronome: Option<Metronome>,

    id_counter: u32,

    is_learning: bool,
    last_midi: Option<Vec<u8>>,

    triggers: Vec<Trigger>,
}

#[allow(dead_code)]
const THRESHOLD: f32 = 0.05;

#[allow(dead_code)]
fn max_abs(b: &[f32]) -> f32 {
    b.iter().map(|v| v.abs())
        .fold(NEG_INFINITY, |a, b| a.max(b))
}

impl Engine {
    pub fn new(config: Config,
               gui_output: Arc<SegQueue<State>>,
               gui_input: Arc<SegQueue<Command>>,
               beat_normal: Vec<f32>,
               beat_emphasis: Vec<f32>) -> Engine {
        let time_signature = TimeSignature::new(4, 4).unwrap();
        let tempo = Tempo::from_bpm(120.0);

        Engine {
            config,

            time: 0,
            time_signature,
            tempo,

            gui_output,
            gui_input,
            loopers: vec![Looper::new(0)],
            active: 0,
            id_counter: 1,

            metronome: Some(Metronome::new(tempo, time_signature,
                                           Sample::from_mono(&beat_normal),
                                           Sample::from_mono(&beat_emphasis))),

            is_learning: false,
            last_midi: None,

            triggers: vec![],
        }
    }

    fn looper_by_id_mut(&mut self, id: u32) -> Option<&mut Looper> {
        self.loopers.iter_mut().find(|l| l.id == id)
    }

    fn commands_from_midi(&self, events: &[MidiEvent]) {
        for e in events {
            println!("midi {:?}", e);

            for m in &self.config.midi_mappings {
                if e.bytes.get(1).map(|b| *b as u32 == m.controller_number).unwrap_or(false) &&
                    e.bytes.get(2).map(|b| *b as u32 == m.data).unwrap_or(false) {
                    if let Some(c) = &m.command {
                        self.gui_input.push(c.clone());
                    }
                }
            }
        }
    }

    fn handle_loop_command(&mut self, lc: &LooperCommand) {
        let loopers: Vec<&mut Looper> = match lc.target_oneof.as_ref().unwrap() {
            TargetOneof::TargetAll(_) => {
                self.loopers.iter_mut().collect()
            }
            TargetOneof::TargetSelected(_) => {
                if let Some(l) = self.looper_by_id_mut(self.active) {
                    vec![l]
                } else {
                    vec![]
                }
            }
            TargetOneof::TargetNumber(t) => {
                if let Some(l) = self.loopers.get_mut(t.looper_number as usize) {
                    vec![l]
                } else {
                    vec![]
                }
            }
        };

        let mut selected = None;

        // TODO: warn if loopers is empty (indicating an invalid selection)
        for l in loopers {
            if let Some(typ) = LooperCommandType::from_i32(lc.command_type) {
                match typ as LooperCommandType {
                    LooperCommandType::EnableReady => {
                        l.transition_to(LooperMode::Ready);
                    }
                    LooperCommandType::EnableRecord => {
                        l.transition_to(LooperMode::Record);
                    },
                    LooperCommandType::EnableOverdub => {
                        l.transition_to(LooperMode::Overdub);
                    },
                    LooperCommandType::EnableMutiply => {
                        // TODO
                    },
                    LooperCommandType::Stop => {
                        l.transition_to(LooperMode::None);
                    }

                    LooperCommandType::EnablePlay => {
                        if l.mode == LooperMode::Record {
                            l.transition_to(LooperMode::Stopping);
                        } else {
                            l.transition_to(LooperMode::Playing);
                        }
                    },
                    LooperCommandType::Select => {
                        selected = Some(l.id);
                    },
                    LooperCommandType::Delete => {
                        l.deleted = true;
                    },

                    LooperCommandType::ReadyOverdubPlay => {
                        if l.samples.is_empty() {
                            l.transition_to(LooperMode::Ready);
                        } else if l.mode == LooperMode::Record || l.mode == LooperMode::Playing {
                            l.transition_to(LooperMode::Overdub);
                        } else {
                            l.transition_to(LooperMode::Playing);
                        }
                    }
                }
            } else {
                // TODO: log this
            }
        }

        if let Some(id) = selected {
            self.active = id;
        }
    }

    fn handle_command(&mut self, command: &Command) {
        if let Some(oneof) = &command.command_oneof {
            match oneof {
                CommandOneof::LooperCommand(lc) => {
                    self.handle_loop_command(lc);
                },
                CommandOneof::GlobalCommand(gc) => {
                    if let Some(typ) = GlobalCommandType::from_i32(gc.command) {
                        match typ as GlobalCommandType {
                            GlobalCommandType::ResetTime => {
                                self.time = 0;
                            },
                            GlobalCommandType::AddLooper => {
                                self.loopers.push(Looper::new(self.id_counter));
                                self.active = self.id_counter;
                                self.id_counter += 1;
                            }
                            GlobalCommandType::EnableLearnMode => {
                                self.is_learning = true;
                            }
                            GlobalCommandType::DisableLearnMode => {
                                self.is_learning = false;
                            }
                        }
                    }
                }
            }
        }
    }

    fn play_loops(&self, outputs: &mut [Vec<f64>; 2]) {
        if self.time >= 0 {
            for looper in &self.loopers {
                if !looper.deleted && (looper.mode == LooperMode::Playing || looper.mode == LooperMode::Overdub) {
                    looper.process_output(self.time as u64, outputs)
                }
            }
        }
    }

    // returns length
    fn measure_len(&self) -> FrameTime {
        let bps = self.tempo.bpm() as f32 / 60.0;
        let mspb = 1000.0 / bps;
        let mspm = mspb * self.time_signature.upper as f32;

        FrameTime::from_ms(mspm as f64)
    }

    // Step 1: Convert midi events to commands
    // Step 2: Handle commands
    // Step 3: Play current samples
    // Step 4: Record
    // Step 5: (async) Update GUI
    pub fn process(&mut self,
                   in_bufs: [&[f32]; 2],
                   out_bufs: &mut [&mut[f32]; 2],
                   met_bufs: &mut [&mut[f32]; 2],
                   frames: u64,
                   midi_events: &[MidiEvent],
    ) {
        // Convert midi events to commands
        if !self.is_learning {
            self.commands_from_midi(midi_events);
            self.last_midi = None;
        } else {
            let new_last = midi_events.last().map(|m| m.bytes.to_vec());
            if new_last.is_some() {
                self.last_midi = new_last;
            }
        }

        // Update our state based on commands
        loop {
            let c = self.gui_input.pop();
            if let Ok(c) = c {
                self.handle_command(&c);
            } else {
                break;
            }
        }

        // Update our time
        let measure_len = self.measure_len();
        {
            if !self.loopers.iter().all(|l| l.mode == LooperMode::None) {
                self.time += frames as i64;

                // Play the metronome
                if let Some(metronome) = &mut self.metronome {
                    metronome.advance(met_bufs);
                }

                let active = self.active;

                // Play our loops
                let buf_len = out_bufs[0].len();
                let mut out_64_vec: [Vec<f64>; 2] = [
                    in_bufs[0].iter().map(|v| *v as f64).collect(),
                    in_bufs[1].iter().map(|v| *v as f64).collect(),
                ];

                self.play_loops(&mut out_64_vec);

                let looper = self.loopers.iter_mut().find(|l| l.id == active).unwrap();

                for i in 0..buf_len {
                    for j in 0..out_64_vec.len() {
                        out_bufs[j][i] = out_64_vec[j][i] as f32
                    }
                }

                // Record input to active loop
                looper.process_input(self.time as u64, &[in_bufs[0], in_bufs[1]]);
            } else {
                if let Some(m) = &mut self.metronome {
                    m.reset();
                }
                self.time = -(measure_len.0 as i64);
            }
        }

        // Update GUI

        // TODO: make this async or non-allocating
        let gui_output = &mut self.gui_output;
        let time = self.time as usize;
        let active = self.active;
        let loop_states: Vec<LoopState> = self.loopers.iter()
            .filter(|l| !l.deleted)
            .map(|l| {
            let len = l.length_in_samples() as usize;

            let t = if len > 0 && (l.mode == LooperMode::Playing || l.mode == LooperMode::Overdub) {
                time % len
            } else {
                0
            };

            LoopState {
                id: l.id,
                mode: l.mode as i32,
                time: FrameTime(t as i64).to_ms() as i64,
                length: FrameTime(len as i64).to_ms() as i64,
                active: l.id == active,
            }
        }).collect();

        // println!("beat {}", self.time_signature.beat_of_measure(self.tempo.beat(
        //     FrameTime(self.time))));
        gui_output.push(State{
            loops: loop_states,
            time: FrameTime(self.time).to_ms() as i64,
            length: 0,
            beat: self.time_signature.beat_of_measure(self.tempo.beat(
                FrameTime(self.time))) as i64,
            bpm: self.tempo.bpm(),
            time_signature_upper: self.time_signature.upper as u64,
            time_signature_lower: self.time_signature.lower as u64,
            learn_mode: self.is_learning,
            last_midi: self.last_midi.as_ref().map(|b| b.clone()).unwrap_or_else(|| vec![]),
        });
    }
}
