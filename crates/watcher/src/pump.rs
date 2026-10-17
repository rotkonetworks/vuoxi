//! The watcher's whole loop body: read raw events, parse, filter, write frames.
//! Generic over reader and writer so the sandbox self-test can run the exact
//! code path with stand-ins.

use std::io::{self, Read, Write};

use crate::ignore::IgnoreTracker;
use crate::parse::parse_events;
use crate::record::Record;

/// Large enough for hundreds of events per `read(2)`.
const READ_BUF: usize = 64 * 1024;

pub struct Pump {
    tracker: IgnoreTracker,
    buf: Vec<u8>,
    records: Vec<Record>,
    frames: Vec<u8>,
}

/// Outcome of one [`Pump::step`].
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// Records written to the output.
    Forwarded(usize),
    /// The source reported end of stream (never from fanotify).
    Eof,
}

impl Pump {
    pub fn new(tracker: IgnoreTracker) -> Self {
        Self {
            tracker,
            buf: vec![0; READ_BUF],
            records: Vec::with_capacity(512),
            frames: Vec::with_capacity(64 * 1024),
        }
    }

    /// One blocking read, then write whatever survives the filter.
    pub fn step<R: Read, W: Write>(&mut self, src: &mut R, out: &mut W) -> io::Result<Step> {
        let n = loop {
            match src.read(&mut self.buf) {
                Ok(n) => break n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        };
        if n == 0 {
            return Ok(Step::Eof);
        }
        self.records.clear();
        parse_events(&self.buf[..n], &mut self.records)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.frames.clear();
        let mut forwarded = 0;
        for rec in &self.records {
            if self.tracker.admit(rec) {
                rec.encode_into(&mut self.frames)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                forwarded += 1;
            }
        }
        if !self.frames.is_empty() {
            out.write_all(&self.frames)?;
        }
        Ok(Step::Forwarded(forwarded))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::{FAN_CREATE, FAN_MODIFY, FAN_ONDIR, build_event};
    use crate::record::Handle;

    fn h(n: u8) -> Handle {
        Handle {
            ty: 1,
            bytes: vec![n; 8],
        }
    }

    #[test]
    fn filters_and_frames() {
        let mut input = Vec::new();
        input.extend(build_event(
            FAN_CREATE | FAN_ONDIR,
            [0; 8],
            Some(&h(9)),
            Some((&h(1), b"target")),
        ));
        input.extend(build_event(
            FAN_MODIFY,
            [0; 8],
            Some(&h(10)),
            Some((&h(9), b"a.o")),
        ));
        input.extend(build_event(
            FAN_MODIFY,
            [0; 8],
            Some(&h(11)),
            Some((&h(1), b"notes.txt")),
        ));
        let mut out = Vec::new();
        let mut pump = Pump::new(IgnoreTracker::default());
        let step = pump.step(&mut input.as_slice(), &mut out).unwrap();
        assert_eq!(step, Step::Forwarded(1));
        let (rec, n) = Record::decode(&out).unwrap().unwrap();
        assert_eq!(rec.name, b"notes.txt");
        assert_eq!(n, out.len());
        assert_eq!(pump.step(&mut &[][..], &mut out).unwrap(), Step::Eof);
    }

    #[test]
    fn bad_bytes_are_an_error() {
        let mut pump = Pump::new(IgnoreTracker::default());
        let junk = [0u8; 64];
        let err = pump.step(&mut &junk[..], &mut Vec::new()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
