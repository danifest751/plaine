#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Val<'a> {
    Str(&'a str),
    Num(u64),
    Bool(bool),
    Null,
    Array,
}

#[derive(Debug, Default)]
pub struct Msg {
    pub id: Option<u64>,
    pub method: Option<String>,
    pub args: Vec<Owned>,
    pub is_error: bool,
    pub error_code: Option<u64>,
    pub error_msg: Option<String>,
    pub result_true: bool,
    /// `"result"` was an object, e.g. `{"status":"OK"}`. Stratum v1 has three spellings
    /// of an accepted share - `true`, an object, or simply `error: null` - and a client
    /// that only knows the first cannot read a pool that uses another. Before this arm
    /// existed the object fell through to the number branch, the whole line failed to
    /// parse and was dropped, so an accepted share counted as neither accepted nor
    /// rejected and its id stayed in `submits` forever.
    pub result_object: bool,
    /// `"result"` was literally `false`: a refusal with no error object attached.
    pub result_false: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owned {
    Str(String),
    Num(u64),
    Bool(bool),
    Other,
}

impl Msg {
    pub fn str_at(&self, i: usize) -> Option<&str> {
        match self.args.get(i) {
            Some(Owned::Str(s)) => Some(s.as_str()),
            _ => None,
        }
    }

    pub fn num_at(&self, i: usize) -> Option<u64> {
        match self.args.get(i) {
            Some(Owned::Num(n)) => Some(*n),
            _ => None,
        }
    }

    pub fn bool_at(&self, i: usize) -> Option<bool> {
        match self.args.get(i) {
            Some(Owned::Bool(b)) => Some(*b),
            _ => None,
        }
    }
}

pub fn parse(line: &str) -> Option<Msg> {
    let b = line.as_bytes();
    let mut p = Parser { b, i: 0 };
    p.ws();
    if p.byte()? != b'{' {
        return None;
    }
    p.i += 1;
    let mut msg = Msg::default();
    p.ws();
    if p.byte()? == b'}' {
        return Some(msg);
    }
    loop {
        p.ws();
        let key = p.string()?;
        p.ws();
        if p.byte()? != b':' {
            return None;
        }
        p.i += 1;
        p.ws();
        match key.as_str() {
            "id" => {
                if p.byte()? == b'n' {
                    p.lit("null")?;
                } else {
                    msg.id = Some(p.number()?);
                }
            }
            "method" => msg.method = Some(p.string()?),
            "params" | "result" => match p.byte()? {
                b'[' => msg.args = p.array()?,
                b'{' => {
                    p.skip_value()?;
                    msg.result_object = true;
                }
                b't' => {
                    p.lit("true")?;
                    msg.result_true = true;
                }
                b'f' => {
                    p.lit("false")?;
                    msg.result_false = true;
                }
                b'n' => p.lit("null")?,
                b'"' => msg.args = vec![Owned::Str(p.string()?)],
                _ => {
                    msg.args = vec![Owned::Num(p.number()?)];
                }
            },
            "error" => match p.byte()? {
                b'n' => p.lit("null")?,
                b'[' => {
                    let v = p.array()?;
                    msg.is_error = true;
                    msg.error_code = match v.first() {
                        Some(Owned::Num(n)) => Some(*n),
                        _ => None,
                    };
                    msg.error_msg = match v.get(1) {
                        Some(Owned::Str(s)) => Some(s.clone()),
                        _ => None,
                    };
                }
                b'"' => {
                    msg.is_error = true;
                    msg.error_msg = Some(p.string()?);
                }
                _ => {
                    p.skip_value()?;
                    msg.is_error = true;
                }
            },
            _ => p.skip_value()?,
        }
        p.ws();
        match p.byte()? {
            b',' => p.i += 1,
            b'}' => return Some(msg),
            _ => return None,
        }
    }
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn byte(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }
    fn ws(&mut self) {
        while matches!(self.byte(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.i += 1;
        }
    }
    fn lit(&mut self, s: &str) -> Option<()> {
        if self.b[self.i..].starts_with(s.as_bytes()) {
            self.i += s.len();
            Some(())
        } else {
            None
        }
    }
    fn number(&mut self) -> Option<u64> {
        let start = self.i;
        if self.byte() == Some(b'-') {
            self.i += 1;
        }
        while matches!(self.byte(), Some(c) if c.is_ascii_digit()) {
            self.i += 1;
        }
        if self.i == start {
            return None;
        }
        core::str::from_utf8(&self.b[start..self.i]).ok()?.parse().ok()
    }
    fn string(&mut self) -> Option<String> {
        if self.byte()? != b'"' {
            return None;
        }
        self.i += 1;
        let mut out = String::new();
        loop {
            let c = self.byte()?;
            self.i += 1;
            match c {
                b'"' => return Some(out),
                b'\\' => {
                    let e = self.byte()?;
                    self.i += 1;
                    out.push(match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'n' => '\n',
                        b't' => '\t',
                        b'r' => '\r',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'u' => {
                            self.i += 4;
                            '\u{fffd}'
                        }
                        _ => return None,
                    });
                }
                _ => out.push(c as char),
            }
        }
    }
    fn array(&mut self) -> Option<Vec<Owned>> {
        if self.byte()? != b'[' {
            return None;
        }
        self.i += 1;
        let mut out = Vec::new();
        self.ws();
        if self.byte()? == b']' {
            self.i += 1;
            return Some(out);
        }
        loop {
            self.ws();
            out.push(match self.byte()? {
                b'"' => Owned::Str(self.string()?),
                b't' => {
                    self.lit("true")?;
                    Owned::Bool(true)
                }
                b'f' => {
                    self.lit("false")?;
                    Owned::Bool(false)
                }
                b'n' => {
                    self.lit("null")?;
                    Owned::Other
                }
                b'[' | b'{' => {
                    self.skip_value()?;
                    Owned::Other
                }
                _ => Owned::Num(self.number()?),
            });
            self.ws();
            match self.byte()? {
                b',' => self.i += 1,
                b']' => {
                    self.i += 1;
                    return Some(out);
                }
                _ => return None,
            }
        }
    }
    fn skip_value(&mut self) -> Option<()> {
        self.ws();
        match self.byte()? {
            b'"' => {
                self.string()?;
            }
            b'[' | b'{' => {
                let open = self.byte()?;
                let close = if open == b'[' { b']' } else { b'}' };
                let mut depth = 0usize;
                loop {
                    match self.byte()? {
                        b'"' => {
                            self.string()?;
                            continue;
                        }
                        c if c == open => depth += 1,
                        c if c == close => {
                            depth -= 1;
                            if depth == 0 {
                                self.i += 1;
                                return Some(());
                            }
                        }
                        _ => {}
                    }
                    self.i += 1;
                }
            }
            b't' => self.lit("true")?,
            b'f' => self.lit("false")?,
            b'n' => self.lit("null")?,
            _ => {
                self.number()?;
            }
        }
        Some(())
    }
}

pub struct Lines<R> {
    r: R,
    buf: Vec<u8>,
    max: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Framed {
    Line(Vec<u8>),
    Idle,
    Eof,
    TooLong,
}

impl<R: std::io::Read> Lines<R> {
    pub fn new(r: R, max: usize) -> Lines<R> {
        Lines { r, buf: Vec::with_capacity(1024), max }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> std::io::Result<Framed> {
        if let Some(l) = self.take_line() {
            return Ok(Framed::Line(l));
        }
        let mut chunk = [0u8; 2048];
        match self.r.read(&mut chunk) {
            Ok(0) => Ok(Framed::Eof),
            Ok(n) => {
                self.buf.extend_from_slice(&chunk[..n]);
                match self.take_line() {
                    Some(l) => Ok(Framed::Line(l)),
                    None if self.buf.len() > self.max => Ok(Framed::TooLong),
                    None => Ok(Framed::Idle),
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Ok(Framed::Idle)
            }
            Err(e) => Err(e),
        }
    }

    fn take_line(&mut self) -> Option<Vec<u8>> {
        let pos = self.buf.iter().position(|&b| b == b'\n')?;

        let end = if pos > 0 && self.buf[pos - 1] == b'\r' { pos - 1 } else { pos };
        let line = self.buf[..end].to_vec();
        self.buf.drain(..=pos);
        Some(line)
    }
}

pub fn hex(bytes: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(D[(b >> 4) as usize] as char);
        s.push(D[(b & 15) as usize] as char);
    }
    s
}

pub fn unhex(s: &str, out: &mut [u8]) -> bool {
    let b = s.as_bytes();
    if b.len() != out.len() * 2 {
        return false;
    }
    for (i, slot) in out.iter_mut().enumerate() {
        let (hi, lo) = (nib(b[2 * i]), nib(b[2 * i + 1]));
        match (hi, lo) {
            (Some(h), Some(l)) => *slot = (h << 4) | l,
            _ => return false,
        }
    }
    true
}

fn nib(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscribe_response_parses() {
        let line = r#"{"id":1,"result":[["mining.notify","mining.set_target"],"00a3f2",5],"error":null}"#;
        let m = parse(line).expect("parses");
        assert_eq!(m.id, Some(1));
        assert!(!m.is_error);

        assert_eq!(m.args[0], Owned::Other);
        assert_eq!(m.str_at(1), Some("00a3f2"));
        assert_eq!(m.num_at(2), Some(5));
    }

    #[test]
    fn notify_and_set_target_parse() {
        let n = parse(
            r#"{"id":null,"method":"mining.notify","params":["0000002b",184602,"abab",true]}"#,
        )
        .expect("parses");
        assert_eq!(n.method.as_deref(), Some("mining.notify"));
        assert_eq!(n.str_at(0), Some("0000002b"));
        assert_eq!(n.num_at(1), Some(184_602));
        assert_eq!(n.bool_at(3), Some(true));

        let t = parse(r#"{"id":null,"method":"mining.set_target","params":["00ff"]}"#).unwrap();
        assert_eq!(t.str_at(0), Some("00ff"));
    }

    #[test]
    fn error_triple_is_read() {
        let e = parse(r#"{"id":7,"result":null,"error":[25,"nonce out of slice",null]}"#).unwrap();
        assert!(e.is_error);
        assert_eq!(e.error_code, Some(25));
        assert_eq!(e.error_msg.as_deref(), Some("nonce out of slice"));
        let ok = parse(r#"{"id":7,"result":true,"error":null}"#).unwrap();
        assert!(!ok.is_error);
        assert!(ok.result_true);
    }

    #[test]
    fn garbage_is_none() {
        assert!(parse("").is_none());
        assert!(parse("not json").is_none());
        assert!(parse(r#"{"id":1"#).is_none());
    }

    struct Flaky(Vec<Result<&'static str, std::io::ErrorKind>>);

    impl std::io::Read for Flaky {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            if self.0.is_empty() {
                return Ok(0);
            }
            match self.0.remove(0) {
                Ok(s) => {
                    out[..s.len()].copy_from_slice(s.as_bytes());
                    Ok(s.len())
                }
                Err(k) => Err(std::io::Error::new(k, "scripted")),
            }
        }
    }

    #[test]
    fn midline_timeout_loses_nothing() {
        let mut l = Lines::new(
            Flaky(vec![
                Ok(r#"{"id":1,"meth"#),
                Err(std::io::ErrorKind::TimedOut),
                Err(std::io::ErrorKind::WouldBlock),
                Ok("od\":\"mining.notify\",\"params\":[]}\n"),
            ]),
            8192,
        );
        assert_eq!(l.next().unwrap(), Framed::Idle, "a partial line is not a line");
        assert_eq!(l.next().unwrap(), Framed::Idle);
        assert_eq!(l.next().unwrap(), Framed::Idle);
        let Framed::Line(line) = l.next().unwrap() else { panic!("the line must arrive whole") };
        let m = parse(core::str::from_utf8(&line).unwrap()).expect("and it must parse");
        assert_eq!(m.method.as_deref(), Some("mining.notify"));
        assert_eq!(l.next().unwrap(), Framed::Eof);
    }

    #[test]
    fn batched_messages_all_delivered() {
        let mut l = Lines::new(Flaky(vec![Ok("{\"a\":1}\n{\"b\":2}\r\n{\"c\":3}\n")]), 8192);
        for expect in [r#"{"a":1}"#, r#"{"b":2}"#, r#"{"c":3}"#] {
            let Framed::Line(line) = l.next().unwrap() else { panic!("expected {expect}") };
            assert_eq!(core::str::from_utf8(&line).unwrap(), expect);
        }
        assert_eq!(l.next().unwrap(), Framed::Eof);
    }

    #[test]
    fn endless_line_is_refused() {
        let mut l = Lines::new(Flaky(vec![Ok("aaaaaaaaaa"), Ok("bbbbbbbbbb")]), 12);
        assert_eq!(l.next().unwrap(), Framed::Idle);
        assert_eq!(l.next().unwrap(), Framed::TooLong);
    }

    #[test]
    fn hex_round_trips() {
        let mut out = [0u8; 4];
        assert!(unhex("deadbeef", &mut out));
        assert_eq!(out, [0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(hex(&out), "deadbeef");
        assert!(!unhex("deadbee", &mut out));
        assert!(!unhex("deadbeeg", &mut out));
    }

}

#[cfg(test)]
mod interop_tests {
    use super::*;

    fn msg(line: &str) -> Msg {
        parse(line).expect("this is a line a real pool sends")
    }

    #[test]
    fn an_object_result_parses_instead_of_killing_the_line() {
        // rplant.xyz answers an accepted share with an object. This used to fall through
        // to the number branch, fail, and drop the whole line: the share counted as
        // neither accepted nor rejected and its id leaked in the pending set.
        let m = msg(r#"{"id":3,"result":{"status":"OK"},"error":null}"#);
        assert_eq!(m.id, Some(3));
        assert!(m.result_object, "an object result has to be seen as one");
        assert!(!m.is_error);
        assert!(!m.result_false);
        assert!(!m.result_true, "it is not the literal true, and must not pretend to be");
    }

    #[test]
    fn a_nested_object_result_parses_too() {
        let m = msg(r#"{"id":9,"result":{"status":"OK","extra":{"a":[1,2,{"b":null}]}},"error":null}"#);
        assert_eq!(m.id, Some(9));
        assert!(m.result_object);
        assert!(!m.is_error);
    }

    #[test]
    fn the_three_spellings_of_yes_and_the_two_of_no() {
        for yes in [
            r#"{"id":1,"result":true,"error":null}"#,
            r#"{"id":1,"result":{"status":"OK"},"error":null}"#,
            r#"{"id":1,"result":null,"error":null}"#,
        ] {
            let m = msg(yes);
            assert!(!(m.is_error || m.result_false), "{yes} is an acceptance");
        }
        for no in [
            r#"{"id":1,"result":null,"error":[21,"stale share"]}"#,
            r#"{"id":1,"result":false,"error":null}"#,
        ] {
            let m = msg(no);
            assert!(m.is_error || m.result_false, "{no} is a refusal");
        }
    }

    #[test]
    fn an_error_array_still_carries_its_code_and_text() {
        let m = msg(r#"{"id":4,"result":null,"error":[21,"stale share"]}"#);
        assert!(m.is_error);
        assert_eq!(m.error_code, Some(21));
        assert_eq!(m.error_msg.as_deref(), Some("stale share"));
    }

    #[test]
    fn a_real_subscribe_reply_still_decodes() {
        let m = msg(r#"{"id":1,"result":[["mining.notify","mining.set_target"],"000000b9",4],"error":null}"#);
        assert_eq!(m.str_at(1), Some("000000b9"), "the extranonce1 is read as written");
        assert_eq!(m.num_at(2), Some(4), "four rollable bytes: the pool sub-slices");
    }
}
