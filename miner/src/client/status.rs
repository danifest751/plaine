//! `--status-format json`: what the miner reports, one JSON object per line on
//! stdout, for a program to read (the desktop wallet's Mining tab does). Every
//! object has an `event` field; nothing else is written to stdout in this mode.
//! Errors and diagnostics stay on stderr as text.
//!
//! ```text
//! {"event":"status","hashrate":24891,"avg":24790,"accepted":12,"rejected":0,"blocks":0,"job":"00002341","height":10912,"uptime":95,"threads":14}
//! {"event":"share","accepted":true,"total_accepted":13,"total_rejected":0}
//! {"event":"share","accepted":false,"total_accepted":13,"total_rejected":1,"code":23,"message":"low difficulty"}
//! {"event":"block","height":10913,"hash":"0000a1..."}
//! {"event":"summary","accepted":13,"rejected":1,"blocks":1,"hashes":2361088,"avg":24850,"uptime":95}
//! ```
//!
//! Rates are whole hashes per second; `uptime` is in seconds; `job` is the job id
//! as the server spells it, or null before the first job.

/// Which output the miner writes on stdout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Format {
    /// Lines for people, as always.
    #[default]
    Text,
    /// One JSON object per line.
    Json,
}

impl Format {
    pub fn parse(s: &str) -> Result<Format, String> {
        match s {
            "text" => Ok(Format::Text),
            "json" => Ok(Format::Json),
            other => Err(format!("status format {other:?} is not text or json")),
        }
    }
}

/// A report, in either format.
#[derive(Clone, Debug, PartialEq)]
pub enum Event<'a> {
    Status {
        hashrate: f64,
        avg: f64,
        accepted: u64,
        rejected: u64,
        blocks: u64,
        job: Option<&'a str>,
        height: u64,
        uptime: u64,
        threads: usize,
    },
    Share {
        accepted: bool,
        total_accepted: u64,
        total_rejected: u64,
        code: Option<i64>,
        message: Option<&'a str>,
    },
    Block {
        height: u64,
        hash: &'a str,
    },
    Summary {
        accepted: u64,
        rejected: u64,
        blocks: u64,
        hashes: u64,
        avg: f64,
        uptime: u64,
    },
}

impl Event<'_> {
    /// The event as one line of JSON.
    pub fn json(&self) -> String {
        let rate = |r: f64| if r.is_finite() && r > 0.0 { r.round() as u64 } else { 0 };
        match self {
            Event::Status { hashrate, avg, accepted, rejected, blocks, job, height, uptime, threads } => {
                format!(
                    "{{\"event\":\"status\",\"hashrate\":{},\"avg\":{},\"accepted\":{accepted},\
                     \"rejected\":{rejected},\"blocks\":{blocks},\"job\":{},\"height\":{height},\
                     \"uptime\":{uptime},\"threads\":{threads}}}",
                    rate(*hashrate),
                    rate(*avg),
                    job.map(quote).unwrap_or_else(|| "null".into()),
                )
            }
            Event::Share { accepted, total_accepted, total_rejected, code, message } => {
                let mut s = format!(
                    "{{\"event\":\"share\",\"accepted\":{accepted},\"total_accepted\":{total_accepted},\
                     \"total_rejected\":{total_rejected}"
                );
                if let Some(c) = code {
                    s.push_str(&format!(",\"code\":{c}"));
                }
                if let Some(m) = message {
                    s.push_str(&format!(",\"message\":{}", quote(m)));
                }
                s.push('}');
                s
            }
            Event::Block { height, hash } => {
                format!("{{\"event\":\"block\",\"height\":{height},\"hash\":{}}}", quote(hash))
            }
            Event::Summary { accepted, rejected, blocks, hashes, avg, uptime } => format!(
                "{{\"event\":\"summary\",\"accepted\":{accepted},\"rejected\":{rejected},\
                 \"blocks\":{blocks},\"hashes\":{hashes},\"avg\":{},\"uptime\":{uptime}}}",
                rate(*avg)
            ),
        }
    }
}

/// `s` as a JSON string literal. Server messages end up here, so every control
/// character is escaped and nothing can break the one-object-per-line framing.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\u{2028}' || c == '\u{2029}' => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::json;

    fn obj(line: &str) -> json::Msg {
        assert!(!line.contains('\n'), "one line: {line}");
        json::parse(line).unwrap_or_else(|| panic!("not JSON the miner itself can read: {line}"))
    }

    #[test]
    fn every_event_is_one_line_of_json() {
        let lines = [
            Event::Status {
                hashrate: 24_890.6,
                avg: f64::NAN,
                accepted: 12,
                rejected: 0,
                blocks: 0,
                job: Some("00002341"),
                height: 10_912,
                uptime: 95,
                threads: 14,
            }
            .json(),
            Event::Status {
                hashrate: 0.0,
                avg: 0.0,
                accepted: 0,
                rejected: 0,
                blocks: 0,
                job: None,
                height: 0,
                uptime: 0,
                threads: 1,
            }
            .json(),
            Event::Share { accepted: true, total_accepted: 1, total_rejected: 0, code: None, message: None }
                .json(),
            Event::Share {
                accepted: false,
                total_accepted: 1,
                total_rejected: 1,
                code: Some(23),
                message: Some("low \"difficulty\"\nsecond line\u{1b}[31m"),
            }
            .json(),
            Event::Block { height: 7, hash: "00ab" }.json(),
            Event::Summary { accepted: 1, rejected: 1, blocks: 1, hashes: 99, avg: 12.4, uptime: 3 }.json(),
        ];
        for l in &lines {
            obj(l);
        }
        assert!(lines[0].contains("\"hashrate\":24891"), "{}", lines[0]);
        assert!(lines[0].contains("\"avg\":0"), "a NaN rate is 0, not invalid JSON: {}", lines[0]);
        assert!(lines[1].contains("\"job\":null"), "{}", lines[1]);
        assert!(lines[3].contains("\\n") && lines[3].contains("\\u001b"), "{}", lines[3]);
    }

    #[test]
    fn the_format_is_text_or_json() {
        assert_eq!(Format::parse("json"), Ok(Format::Json));
        assert_eq!(Format::parse("text"), Ok(Format::Text));
        assert!(Format::parse("xml").is_err());
        assert_eq!(Format::default(), Format::Text);
    }
}
