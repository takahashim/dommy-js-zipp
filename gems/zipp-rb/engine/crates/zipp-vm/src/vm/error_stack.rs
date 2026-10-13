//! `Error.prototype.stack`: the frames an Error was created in, as V8 prints
//! them (`at name (script:line:col)`).
//!
//! An Error records `(func id, ip)` for each live frame when it is allocated
//! (`capture_error_frames`, called wherever an object gains [[ErrorData]]);
//! the string is built when `stack` is read. A frame's position is the
//! statement it was executing (`FuncProto::positions`), and its script is the
//! name the host gave the code (`FuncProto::script`).
#![allow(unused_imports)]
use super::*;
use crate::value::Value;

/// How many frames an Error keeps: V8's default `Error.stackTraceLimit`.
const STACK_TRACE_LIMIT: usize = 10;

/// What a frame of a script with no name reports as its file. dommy (and
/// other hosts) read it as "the document".
const ANONYMOUS_SCRIPT: &str = "<anonymous>";

impl<'p> Vm<'p> {
    /// Record the live frames for the Error at heap index `obj`.
    pub(crate) fn capture_error_frames(&mut self, obj: u32) {
        let Some(top) = self.frames.len().checked_sub(1) else {
            return;
        };
        let mut frames = Vec::with_capacity(STACK_TRACE_LIMIT.min(self.frames.len()));
        for (i, frame) in self.frames.iter().enumerate().rev() {
            if frames.len() == STACK_TRACE_LIMIT {
                break;
            }
            // The innermost interpreted frame's pc is `interp_pc` (its
            // `frames[..].ip` is stale while it runs); a caller's `ip` is
            // the instruction after its call.
            let ip = if i == top && (self.interp_pc >> 32) as usize == top {
                (self.interp_pc & u32::MAX as u64) as u32
            } else if i == top {
                frame.ip as u32
            } else {
                (frame.ip as u32).saturating_sub(1)
            };
            frames.push((frame.func, ip));
        }
        self.error_frames.insert(obj, frames);
    }

    /// The `stack` string of the Error at heap index `obj`: its
    /// `name: message` header, then a line per frame that names a function
    /// or has a position.
    pub(crate) fn error_stack_string(&mut self, obj: u32) -> Result<String, Thrown> {
        let mut out = self.error_stack_header(obj)?;
        for line in self.error_frame_lines(obj) {
            out.push_str("\n    ");
            out.push_str(&line);
        }
        Ok(out)
    }

    /// The frame lines of the Error at heap index `obj` (`at …`), innermost
    /// first; empty when none were recorded.
    pub(crate) fn error_frame_lines(&self, obj: u32) -> Vec<String> {
        let Some(frames) = self.error_frames.get(&obj) else {
            return Vec::new();
        };
        frames
            .iter()
            .filter_map(|&(func, ip)| self.error_frame_line(func, ip))
            .collect()
    }

    fn error_frame_line(&self, func: u32, ip: u32) -> Option<String> {
        let f = self.func(func as usize);
        // Compiler-internal names (`<script>`, `<arrow>`) are anonymous.
        let name = if f.name.starts_with('<') { "" } else { f.name.as_str() };
        let pos = f.positions.iter().rev().find(|p| p.ip <= ip);
        let script = f.script.as_deref().unwrap_or(ANONYMOUS_SCRIPT);
        Some(match (name.is_empty(), pos) {
            (false, Some(p)) => format!("at {name} ({script}:{}:{})", p.line, p.col),
            (true, Some(p)) => format!("at {script}:{}:{}", p.line, p.col),
            (false, None) => format!("at {name} ({script})"),
            // Synthetic code with no name and no position (a host's empty
            // root program) is not a frame the page can find.
            (true, None) => return None,
        })
    }

    /// ErrorToString's `name: message`, read now (as V8 reads it when the
    /// stack is first formatted).
    fn error_stack_header(&mut self, obj: u32) -> Result<String, Thrown> {
        let e = Value::heap(obj);
        let name = self.get_prop(e, "name")?;
        let name = if name == Value::UNDEFINED { "Error".to_string() } else { self.display(name) };
        let message = self.get_prop(e, "message")?;
        let message = if message == Value::UNDEFINED { String::new() } else { self.display(message) };
        Ok(match (name.is_empty(), message.is_empty()) {
            (_, true) => name,
            (true, false) => message,
            (false, false) => format!("{name}: {message}"),
        })
    }
}
