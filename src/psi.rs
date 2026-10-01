//! Parsing `/proc/pressure/memory` without allocating.
//!
//! ```text
//! some avg10=0.00 avg60=0.00 avg300=0.00 total=0
//! full avg10=0.00 avg60=0.00 avg300=0.00 total=0
//! ```

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Line {
    pub avg10: f32,
    pub avg60: f32,
    pub avg300: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Pressure {
    pub some: Line,
    pub full: Line,
}

fn parse_line(rest: &str) -> Option<Line> {
    let mut l = Line::default();
    let (mut a10, mut a60, mut a300) = (false, false, false);
    for field in rest.split_ascii_whitespace() {
        let (k, v) = field.split_once('=')?;
        match k {
            "avg10" => (l.avg10, a10) = (v.parse().ok()?, true),
            "avg60" => (l.avg60, a60) = (v.parse().ok()?, true),
            "avg300" => (l.avg300, a300) = (v.parse().ok()?, true),
            _ => {}
        }
    }
    (a10 && a60 && a300).then_some(l)
}

/// Both lines are required: `full` is the signal that matters, and a file
/// without it (pre-5.13 CPU PSI, or a truncated read) must not read as zero.
pub fn parse(buf: &[u8]) -> Option<Pressure> {
    let text = core::str::from_utf8(buf).ok()?;
    let (mut some, mut full) = (None, None);
    for line in text.lines() {
        if let Some(r) = line.strip_prefix("some ") {
            some = parse_line(r);
        } else if let Some(r) = line.strip_prefix("full ") {
            full = parse_line(r);
        }
    }
    Some(Pressure { some: some?, full: full? })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_file() {
        let p = parse(
            b"some avg10=41.27 avg60=38.90 avg300=12.03 total=123456789\n\
              full avg10=30.10 avg60=29.55 avg300=9.80 total=98765432\n",
        )
        .unwrap();
        assert_eq!(p.some.avg10, 41.27);
        assert_eq!(p.full.avg10, 30.10);
        assert_eq!(p.full.avg60, 29.55);
        assert_eq!(p.full.avg300, 9.80);
    }

    #[test]
    fn idle() {
        let p = parse(b"some avg10=0.00 avg60=0.00 avg300=0.00 total=0\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n").unwrap();
        assert_eq!(p, Pressure::default());
    }

    #[test]
    fn rejects_partial() {
        assert!(parse(b"some avg10=1.00 avg60=0.00 avg300=0.00 total=0\n").is_none());
        assert!(parse(b"some avg10=1.00 avg60=0.00 avg300=0.00 total=0\nfull avg10=2.0").is_none());
        assert!(parse(b"some avg10=x avg60=0 avg300=0 total=0\nfull avg10=0 avg60=0 avg300=0 total=0").is_none());
        assert!(parse(b"").is_none());
        assert!(parse(&[0xff, 0xfe]).is_none());
    }
}
