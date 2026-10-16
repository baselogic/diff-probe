#![forbid(unsafe_code)]

use std::env;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

const PATCH_LIMIT: u64 = 16 * 1024 * 1024;
const FILE_LIMIT: u64 = 64 * 1024 * 1024;
const PATCH_LINES: usize = 250_000;
const SOURCE_LINES: usize = 1_000_000;
const SEARCH_BUDGET: usize = 8_000_000;
const OUTPUT_LIMIT: usize = 100_000;
const PREVIEW_BYTES: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Eol { Lf, CrLf, None, Unknown }
impl Eol {
    const fn name(self) -> &'static str {
        match self { Self::Lf => "LF", Self::CrLf => "CRLF", Self::None => "no EOL", Self::Unknown => "unknown" }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Change { Context, Delete, Add }
struct DiffLine { kind: Change, text: Vec<u8>, eol: Eol }
struct Hunk { start: usize, count: usize, new_start: usize, new_count: usize, lines: Vec<DiffLine> }
impl Hunk {
    fn old(&self) -> Vec<&DiffLine> { self.lines.iter().filter(|l| l.kind != Change::Add).collect() }
    fn expected(&self) -> usize { if self.count == 0 { self.start } else { self.start - 1 } }
}
#[derive(Default)]
struct Section {
    old: Option<String>, new: Option<String>, old_header: bool, new_header: bool,
    hunks: Vec<Hunk>, metadata: bool,
}
impl Section {
    fn present(&self) -> bool { self.old_header || self.new_header || self.metadata || !self.hunks.is_empty() }
}
#[derive(Clone, Copy)]
struct Raw<'a> { bytes: &'a [u8], terminated: bool }
fn raw_lines(bytes: &[u8]) -> Vec<Raw<'_>> {
    bytes.split_inclusive(|b| *b == b'\n').map(|part| match part.strip_suffix(b"\n") {
        Some(body) => Raw { bytes: body, terminated: true },
        None => Raw { bytes: part, terminated: false },
    }).collect()
}
fn structural<'a>(line: Raw<'a>, transport: &mut Option<Eol>) -> Result<&'a [u8], String> {
    if !line.terminated { return Err("diff header is missing a final LF".into()); }
    let (bytes, eol) = match line.bytes.strip_suffix(b"\r") {
        Some(b) => (b, Eol::CrLf), None => (line.bytes, Eol::Lf),
    };
    if transport.is_some_and(|prev| prev != eol) { return Err("mixed line endings in patch headers".into()); }
    *transport = Some(eol);
    Ok(bytes)
}
fn decimal(text: &str) -> Result<(usize, &str), String> {
    let n = text.bytes().take_while(u8::is_ascii_digit).count();
    if n == 0 { return Err("missing number in hunk header".into()); }
    let value = text[..n].parse().map_err(|_| "number out of range in hunk header")?;
    Ok((value, &text[n..]))
}
fn range(s: &str, sign: char) -> Result<(usize, usize, &str), String> {
    let s = s.strip_prefix(sign).ok_or("incorrect sign in hunk header")?;
    let (start, tail) = decimal(s)?;
    let (count, rest) = if let Some(tail) = tail.strip_prefix(',') { decimal(tail)? } else { (1, tail) };
    if (count > 0 && start == 0) || start.checked_add(count).is_none() { return Err("invalid range in hunk header".into()); }
    Ok((start, count, rest))
}
fn header(bytes: &[u8]) -> Result<(usize, usize, usize, usize), String> {
    let s = std::str::from_utf8(bytes).map_err(|_| "hunk header is not UTF-8")?;
    let s = s.strip_prefix("@@ ").ok_or("invalid hunk header")?;
    let (old_start, old_count, s) = range(s, '-')?;
    let (new_start, new_count, s) = range(s.strip_prefix(' ').ok_or("missing space between hunk ranges")?, '+')?;
    let after = s.strip_prefix(" @@").ok_or("missing closing @@")?;
    if (!after.is_empty() && !after.starts_with(' ')) || (old_count == 0 && new_count == 0) { return Err("invalid hunk header".into()); }
    Ok((old_start, old_count, new_start, new_count))
}
// Only --- and +++ paths are interpreted; Git metadata is outside this contract.
fn filename(raw: &[u8], prefix: &[u8]) -> Result<Option<String>, String> {
    let decoded = if raw.starts_with(b"\"") {
        let mut v = Vec::new();
        let mut i = 1;
        while i < raw.len() && raw[i] != b'"' {
            if raw[i] != b'\\' { v.push(raw[i]); i += 1; continue; }
            i += 1;
            let b = *raw.get(i).ok_or("incomplete path escape")?;
            match b {
                b'\\' | b'"' => v.push(b), b't' => v.push(b'\t'), b'n' => v.push(b'\n'),
                b'r' => v.push(b'\r'), b'a' => v.push(7), b'b' => v.push(8),
                b'f' => v.push(12), b'v' => v.push(11),
                b'0'..=b'7' => {
                    let next = raw.get(i + 1..i + 3).ok_or("incomplete octal path escape")?;
                    if !next.iter().all(|n| (b'0'..=b'7').contains(n)) { return Err("invalid octal path escape".into()); }
                    let value = (u16::from(b - b'0') << 6) | (u16::from(next[0] - b'0') << 3) | u16::from(next[1] - b'0');
                    v.push(u8::try_from(value).map_err(|_| "octal path escape out of range")?);
                    i += 2;
                }
                _ => return Err("unrecognized path escape".into()),
            }
            i += 1;
        }
        if raw.get(i) != Some(&b'"') || !(raw.get(i + 1..).unwrap_or_default().is_empty() || raw[i + 1..].starts_with(b"\t")) {
            return Err("invalid quotes or suffix in path".into());
        }
        v
    } else { raw.split(|b| *b == b'\t').next().unwrap_or_default().to_vec() };
    if decoded == b"/dev/null" { return Ok(None); }
    let name = decoded.strip_prefix(prefix).unwrap_or(&decoded);
    let name = String::from_utf8(name.to_vec()).map_err(|_| "path is not UTF-8")?;
    if name.is_empty() { return Err("empty path".into()); }
    Ok(Some(name))
}
fn push_section(files: &mut Vec<Section>, section: &mut Section) {
    if section.present() { files.push(std::mem::take(section)); }
}
fn parse(bytes: &[u8]) -> Result<(Vec<Section>, Eol), String> {
    if bytes.iter().filter(|&&b| b == b'\n').take(PATCH_LINES + 1).count() > PATCH_LINES {
        return Err("patch exceeds the line limit".into());
    }
    let lines = raw_lines(bytes);
    let mut files = Vec::new();
    let mut current = Section::default();
    let mut transport = None;
    let mut active = false;
    // Git email patches and git show output may have a free-form preamble.
    let mail = bytes.starts_with(b"From ");
    let mut preamble = mail || bytes.starts_with(b"commit ");
    let mut mail_separator = !mail;
    let mut i = 0;
    while i < lines.len() {
        let raw = lines[i];
        let check = raw.bytes.strip_suffix(b"\r").unwrap_or(raw.bytes);
        if preamble {
            if !mail_separator {
                if check == b"---" { mail_separator = true; }
                i += 1;
                continue;
            }
            if !check.starts_with(b"diff --git ") { i += 1; continue; }
            preamble = false;
        }
        if check.starts_with(b"diff --git ") {
            structural(raw, &mut transport)?;
            push_section(&mut files, &mut current);
            current.metadata = true;
            active = true;
        } else if check.starts_with(b"--- ") {
            if active && current.old_header {
                push_section(&mut files, &mut current);
            }
            let data = structural(raw, &mut transport)?;
            current.old = filename(&data[4..], b"a/")?;
            current.old_header = true;
            active = true;
        } else if active && check.starts_with(b"+++ ") {
            let data = structural(raw, &mut transport)?;
            if !current.old_header || current.new_header { return Err("incomplete or duplicate --- / +++ headers".into()); }
            current.new = filename(&data[4..], b"b/")?;
            current.new_header = true;
        } else if active && check.starts_with(b"@@ ") {
            let data = structural(raw, &mut transport)?;
            if !current.new_header { return Err("hunk without --- / +++ headers".into()); }
            let (start, count, new_start, new_count) = header(data)?;
            let (mut old_seen, mut new_seen) = (0usize, 0usize);
            let mut hunk = Hunk { start, count, new_start, new_count, lines: Vec::new() };
            i += 1;
            while old_seen < count || new_seen < new_count {
                let entry = lines.get(i).ok_or("truncated hunk")?;
                if !entry.terminated { return Err("truncated hunk record".into()); }
                let (&mark, text) = entry.bytes.split_first().ok_or("empty line in hunk")?;
                let kind = match mark {
                    b' ' => { old_seen += 1; new_seen += 1; Change::Context }
                    b'-' => { old_seen += 1; Change::Delete }
                    b'+' => { new_seen += 1; Change::Add }
                    _ => return Err("invalid hunk line marker".into()),
                };
                if old_seen > count || new_seen > new_count { return Err("too many lines in hunk".into()); }
                let (text, eol) = match transport {
                    Some(Eol::CrLf) => {
                        let body = text.strip_suffix(b"\r").ok_or("hunk transport is not CRLF")?;
                        match body.strip_suffix(b"\r") { Some(t) => (t, Eol::CrLf), None => (body, Eol::Unknown) }
                    }
                    _ => match text.strip_suffix(b"\r") { Some(t) => (t, Eol::CrLf), None => (text, Eol::Lf) },
                };
                hunk.lines.push(DiffLine { kind, text: text.to_vec(), eol });
                i += 1;
                if let Some(next) = lines.get(i) {
                    let marker = next.bytes.strip_suffix(b"\r").unwrap_or(next.bytes);
                    if marker == b"\\ No newline at end of file" {
                        let normalized = structural(*next, &mut transport)?;
                        if normalized != b"\\ No newline at end of file" { return Err("invalid no-newline marker".into()); }
                        let last = hunk.lines.last_mut().ok_or("orphaned no-newline marker")?;
                        if (last.kind != Change::Add && old_seen != count)
                            || (last.kind != Change::Delete && new_seen != new_count) {
                            return Err("no-newline marker before end of hunk side".into());
                        }
                        if last.eol == Eol::CrLf { last.text.push(b'\r'); }
                        last.eol = Eol::None;
                        i += 1;
                    }
                }
            }
            current.hunks.push(hunk);
            continue;
        } else if mail && active && check == b"-- " && !current.hunks.is_empty() {
            // The remainder is an email signature; a subsequent diff --git starts another section.
            push_section(&mut files, &mut current);
            active = false;
            preamble = true;
            mail_separator = false;
        } else if check.starts_with(b"Binary files ") || (active && check.starts_with(b"GIT binary patch")) {
            current.metadata = true;
            active = true;
        } else if active && current.new_header && current.hunks.is_empty() {
            return Err("unexpected record before the first hunk".into());
        } else if active && !current.hunks.is_empty()
            && matches!(check.first().copied(), Some(b'+' | b'-' | b' ' | b'\\'))
            && check != b"-- " {
            return Err("stray record outside a hunk".into());
        }
        i += 1;
    }
    push_section(&mut files, &mut current);
    if files.is_empty() { return Err("no files found in patch".into()); }
    for file in &files {
        if file.old_header != file.new_header { return Err("unpaired --- or +++ header in section".into()); }
        if file.old_header && file.old.is_none() && file.new.is_none() { return Err("both sides are /dev/null".into()); }
        if file.old_header {
            let mut previous_old_end = 0;
            let mut previous_new_end = 0;
            for hunk in &file.hunks {
                if (file.old.is_none() && hunk.count != 0)
                    || (file.new.is_none() && hunk.new_count != 0) {
                    return Err("hunk is incompatible with /dev/null".into());
                }
                let new_at = if hunk.new_count == 0 { hunk.new_start } else { hunk.new_start - 1 };
                if hunk.expected() < previous_old_end || new_at < previous_new_end {
                    return Err("overlapping or out-of-order hunks".into());
                }
                previous_old_end = hunk.expected() + hunk.count;
                previous_new_end = new_at + hunk.new_count;
            }
        }
    }
    Ok((files, transport.unwrap_or(Eol::Unknown)))
}

struct SourceLine<'a> { text: &'a [u8], eol: Eol }
fn source_lines(bytes: &[u8]) -> Vec<SourceLine<'_>> {
    raw_lines(bytes).into_iter().map(|line| {
        let (text, eol) = if !line.terminated { (line.bytes, Eol::None) }
            else if let Some(t) = line.bytes.strip_suffix(b"\r") { (t, Eol::CrLf) }
            else { (line.bytes, Eol::Lf) };
        SourceLine { text, eol }
    }).collect()
}
#[derive(Clone, Copy)]
enum Quality { Strict, Unknown, Content }
#[derive(Clone, Copy)]
enum Location { Hit { at: usize, count: u8, quality: Quality }, Insertion(usize), Missing, OutOfRange, Limited }
fn matches_at(src: &[SourceLine<'_>], old: &[&DiffLine], at: usize, strict: bool, budget: &mut usize) -> Option<bool> {
    let end = at.checked_add(old.len())?;
    let Some(block) = src.get(at..end) else { return Some(false); };
    for (actual, expected) in block.iter().zip(old) {
        if *budget == 0 { return None; }
        *budget -= 1;
        if actual.text != expected.text || (strict && match expected.eol {
            Eol::Unknown => actual.eol == Eol::None,
            other => actual.eol != other,
        }) { return Some(false); }
    }
    Some(true)
}
fn search(src: &[SourceLine<'_>], old: &[&DiffLine], strict: bool, budget: &mut usize) -> (usize, usize, bool) {
    let mut count = 0;
    let mut first = 0;
    let Some(last) = src.len().checked_sub(old.len()) else { return (0, 0, false); };
    for at in 0..=last {
        match matches_at(src, old, at, strict, budget) {
            Some(true) => { count += 1; if count == 1 { first = at; } else if count == 2 { return (count, first, false); } }
            Some(false) => {},
            None => return (count, first, true),
        }
    }
    (count, first, false)
}
fn locate(hunk: &Hunk, src: &[SourceLine<'_>], budget: &mut usize) -> Location {
    let at = hunk.expected();
    if hunk.count == 0 { return if at <= src.len() { Location::Insertion(at) } else { Location::OutOfRange }; }
    let old = hunk.old();
    let q = if old.iter().any(|l| l.eol == Eol::Unknown) { Quality::Unknown } else { Quality::Strict };
    match matches_at(src, &old, at, true, budget) {
        Some(true) => return Location::Hit { at, count: 1, quality: q },
        None => return Location::Limited,
        Some(false) => {},
    }
    let (count, first, limited) = search(src, &old, true, budget);
    if count == 2 { return Location::Hit { at: first, count: 2, quality: q }; }
    if limited { return Location::Limited; }
    if count == 1 { return Location::Hit { at: first, count: 1, quality: q }; }
    match matches_at(src, &old, at, false, budget) {
        Some(true) => return Location::Hit { at, count: 1, quality: Quality::Content },
        None => return Location::Limited,
        Some(false) => {},
    }
    let (count, first, limited) = search(src, &old, false, budget);
    if count == 2 { return Location::Hit { at: first, count: 2, quality: Quality::Content }; }
    if limited { return Location::Limited; }
    if count == 1 { return Location::Hit { at: first, count: 1, quality: Quality::Content }; }
    if at.saturating_add(old.len()) > src.len() { Location::OutOfRange } else { Location::Missing }
}

fn visible(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        let (prefix, invalid) = match std::str::from_utf8(rest) {
            Ok(s) => (s, 0),
            Err(e) => (std::str::from_utf8(&rest[..e.valid_up_to()]).unwrap_or(""), e.error_len().unwrap_or(rest.len() - e.valid_up_to())),
        };
        for c in prefix.chars() {
            if c == '\t' { out.push_str("\\t"); }
            else if c.is_control() || matches!(c, '\u{061C}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}') {
                let _ = write!(out, "\\u{{{:X}}}", c as u32);
            } else { out.push(c); }
        }
        rest = &rest[prefix.len()..];
        for &b in &rest[..invalid] { let _ = write!(out, "\\x{b:02X}"); }
        rest = &rest[invalid..];
    }
    out
}
fn preview(bytes: &[u8]) -> String {
    let mut end = bytes.len().min(PREVIEW_BYTES);
    if end < bytes.len()
        && let Err(e) = std::str::from_utf8(&bytes[..end])
        && e.error_len().is_none() {
        end = e.valid_up_to();
    }
    let mut text = visible(&bytes[..end]);
    if end < bytes.len() { text.push_str(" …"); }
    text
}
fn color(text: &str, code: &str, enabled: bool) -> String {
    if enabled { format!("\x1b[{code}m{text}\x1b[0m") } else { text.to_owned() }
}
fn displacement(at: usize, expected: usize) -> String {
    if at == expected { String::new() }
    else if at > expected { format!(" (shifted +{})", at - expected) }
    else { format!(" (shifted -{})", expected - at) }
}
fn status(location: Location, expected: usize) -> String {
    match location {
        Location::Hit { at, count: 2, quality } => {
            let eol = match quality {
                Quality::Strict => "",
                Quality::Content => "; EOL differs",
                Quality::Unknown => "; original EOL unknown",
            };
            format!("AMBIGUOUS: at least 2 matches; first at line {}{eol}", at + 1)
        },
        Location::Hit { at, quality: Quality::Content, .. } => format!("CONTENT ONLY: EOL differs, line {}{}", at + 1, displacement(at, expected)),
        Location::Hit { at, quality: Quality::Unknown, .. } => format!("CONTENT MATCH: original EOL unknown, line {}{}", at + 1, displacement(at, expected)),
        Location::Hit { at, .. } if at == expected => format!("MATCH: line {}", at + 1),
        Location::Hit { at, .. } => format!("SHIFTED: line {}{}", at + 1, displacement(at, expected)),
        Location::Insertion(p) => format!("INSERTION: no preimage; after line {p}"),
        Location::Missing => "NOT FOUND: content differs".into(),
        Location::OutOfRange => "RANGE OUTSIDE FILE".into(),
        Location::Limited => "UNDETERMINED: search budget exhausted".into(),
    }
}
fn mismatch(hunk: &Hunk, src: &[SourceLine<'_>], at: usize) -> Option<String> {
    for (offset, old) in hunk.old().iter().enumerate() {
        let n = at.checked_add(offset)?;
        let Some(actual) = src.get(n) else { return Some(format!("line {}: end of file (EOF)", n + 1)); };
        if old.text != actual.text {
            let col = old.text.iter().zip(actual.text).position(|(a, b)| a != b)
                .unwrap_or(old.text.len().min(actual.text.len()));
            let byte = |b: Option<&u8>| b.map_or_else(|| "end of line".into(), |v| format!("0x{v:02X}"));
            return Some(format!("line {}, byte {}: patch {}, file {}", n + 1, col + 1, byte(old.text.get(col)), byte(actual.text.get(col))));
        }
        if old.eol != Eol::Unknown && old.eol != actual.eol || old.eol == Eol::Unknown && actual.eol == Eol::None {
            return Some(format!("line {}: patch EOL {}, file EOL {}", n + 1, old.eol.name(), actual.eol.name()));
        }
    }
    None
}
fn show(out: &mut impl Write, hunk: &Hunk, src: &[SourceLine<'_>], where_: Location, context: usize, colored: bool) -> io::Result<()> {
    let expected = hunk.expected();
    let at = match where_ { Location::Hit { at, .. } | Location::Insertion(at) => at, _ => expected };
    writeln!(out, "@@ -{},{} +{},{} @@  {}", hunk.start, hunk.count, hunk.new_start, hunk.new_count, status(where_, expected))?;
    if matches!(where_, Location::OutOfRange) && at >= src.len() && (hunk.count > 0 || at > src.len()) {
        writeln!(out, "{}", color(&format!("!    outside file: position {}, {} lines total", at + usize::from(hunk.count > 0), src.len()), "33", colored))?;
        return Ok(());
    }
    let until = at.saturating_add(hunk.count).saturating_add(context).min(src.len());
    let old_end = at.saturating_add(hunk.count);
    let mut changes = hunk.lines.iter().peekable();
    let mut additions = 0usize;
    // The final position is an insertion anchor, even when it equals src.len().
    #[allow(clippy::needless_range_loop)]
    for i in at.saturating_sub(context)..=until {
        if hunk.count == 0 && i == at {
            writeln!(out, "{}", color(&format!(">      insert after line {at}"), "32", colored))?;
        }
        // Emit additions before the next preimage line.
        if i >= at && i <= old_end {
            while let Some(added) = changes.next_if(|line| line.kind == Change::Add) {
                if additions < 8 {
                    writeln!(out, "{}", color(&format!("+        {}", preview(&added.text)), "32", colored))?;
                }
                additions += 1;
            }
        }
        if i == until { break; }
        let old_line = i.checked_sub(at).filter(|offset| *offset < hunk.count)
            .and_then(|_| changes.next());
        let (prefix, tone) = match old_line {
            Some(_) if !matches!(where_, Location::Hit { count: 1, .. }) => ('!', "33"),
            Some(line) if line.kind == Change::Delete => ('-', "31"),
            Some(_) => ('=', "36"),
            None => (' ', "0"),
        };
        let mut line = format!("{prefix} {:>6}  {}", i + 1, preview(src[i].text));
        if old_line.is_some() && matches!(where_, Location::Hit { quality: Quality::Content | Quality::Unknown, .. }) {
            let _ = write!(line, " [{}]", src[i].eol.name());
        }
        writeln!(out, "{}", color(&line, tone, colored && tone != "0"))?;
    }
    if matches!(where_, Location::OutOfRange) {
        writeln!(out, "{}", color("!    EOF before the hunk ends", "33", colored))?;
        // Additions remaining after the preimage reaches EOF still belong to the hunk.
        for added in changes.filter(|line| line.kind == Change::Add) {
            if additions < 8 {
                writeln!(out, "{}", color(&format!("+        {}", preview(&added.text)), "32", colored))?;
            }
            additions += 1;
        }
    }
    if let Some(detail) = mismatch(hunk, src, at)
        && !matches!(where_, Location::Hit { quality: Quality::Strict | Quality::Unknown, .. }) {
        writeln!(out, "  first difference: {detail}")?;
    }
    if additions > 8 { writeln!(out, "  ... {} added lines omitted", additions - 8)?; }
    Ok(())
}

fn checked_path(root: &Path, name: &str) -> Result<Option<PathBuf>, String> {
    if name.is_empty() || name.contains('\0')
        || name.split('/').any(|p| p.is_empty() || p == "." || p == "..") {
        return Err(format!("invalid relative path: {}", visible(name.as_bytes())));
    }
    #[cfg(windows)]
    if name.contains('\\') || name.contains(':') {
        return Err(format!("invalid relative path: {}", visible(name.as_bytes())));
    }
    let rel = Path::new(name);
    if !rel.components().all(|c| matches!(c, Component::Normal(_))) { return Err("absolute path or path outside --root".into()); }
    #[cfg(windows)]
    {
        // Windows device names and suffix restrictions do not apply to Unix.
        for component in rel.components() {
            let part = component.as_os_str().to_string_lossy();
            if part.ends_with(' ') || part.ends_with('.') { return Err("path has an ambiguous Windows suffix".into()); }
            let base = part.split('.').next().unwrap_or("").trim_end_matches([' ', '.']).to_ascii_uppercase();
            let reserved = matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
                || (base.len() == 4 && (base.starts_with("COM") || base.starts_with("LPT"))
                    && base.as_bytes()[3].is_ascii_digit() && base.as_bytes()[3] != b'0');
            if reserved { return Err("reserved Windows device name".into()); }
        }
    }
    let path = root.join(rel);
    match path.canonicalize() {
        Ok(resolved_path) if resolved_path.starts_with(root) => Ok(Some(resolved_path)),
        Ok(_) => Err("resolved path is outside --root".into()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // Missing paths must still have their existing ancestor inside --root.
            for ancestor in path.ancestors().skip(1) {
                match ancestor.symlink_metadata() {
                    Ok(_) => {
                        let resolved = ancestor.canonicalize().map_err(|e| format!("{}: {e}", ancestor.display()))?;
                        if resolved.starts_with(root) { return Ok(None); }
                        return Err("resolved path is outside --root".into());
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {},
                    Err(e) => return Err(format!("{}: {e}", ancestor.display())),
                }
            }
            Err("could not resolve an existing path ancestor".into())
        },
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}
fn read_limited(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    // A static FIFO can block during File::open; concurrent replacement remains possible.
    let metadata = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(format!("not a regular file or exceeds {limit} bytes: {}", path.display()));
    }
    let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.len() > limit { return Err(format!("not a regular file or exceeds {limit} bytes: {}", path.display())); }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > limit { return Err("file grew while being read".into()); }
    Ok(bytes)
}
#[derive(Clone, Copy)]
enum Colors { Auto, Always, Never }
struct Config { patch: PathBuf, root: PathBuf, context: usize, colors: Colors }
fn options() -> Result<Option<Config>, String> {
    let mut patch = None;
    let mut root = PathBuf::from(".");
    let mut context = 5;
    let mut colors = Colors::Auto;
    let mut args = env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--help") => return Ok(None),
            Some("--root") => root = args.next().ok_or("missing value for --root")?.into(),
            Some("--context") => {
                context = args.next().and_then(|v| v.into_string().ok()).ok_or("missing value for --context")?
                    .parse().map_err(|_| "--context requires an integer")?;
                if context > 1000 { return Err("--context must be between 0 and 1000".into()); }
            }
            Some("--color") => colors = match args.next().and_then(|v| v.into_string().ok()).as_deref() {
                Some("auto") => Colors::Auto, Some("always") => Colors::Always, Some("never") => Colors::Never,
                _ => return Err("--color must be auto, always, or never".into()),
            },
            Some(flag) if flag.starts_with('-') => return Err(format!("unknown option: {flag}")),
            _ if patch.is_none() => patch = Some(PathBuf::from(&arg)),
            _ => return Err("only one PATCH argument is allowed".into()),
        }
    }
    Ok(Some(Config { patch: patch.ok_or("missing PATCH argument")?, root, context, colors }))
}
fn execute(c: &Config, out: &mut impl Write) -> Result<bool, String> {
    let patch = read_limited(&c.patch, PATCH_LIMIT)?;
    let (sections, transport) = parse(&patch)?;
    let root = c.root.canonicalize().map_err(|e| format!("--root: {e}"))?;
    if !root.is_dir() { return Err("--root is not a directory".into()); }
    let colors = match c.colors { Colors::Always => true, Colors::Never => false,
        Colors::Auto => io::stdout().is_terminal() && env::var_os("NO_COLOR").is_none() };
    let mut rows = sections.len().saturating_mul(3);
    for hunk in sections.iter().flat_map(|section| &section.hunks) {
        rows = rows.saturating_add(hunk.count).saturating_add(c.context.saturating_mul(2)).saturating_add(16);
        if rows > OUTPUT_LIMIT { return Err("output limit exceeded; reduce --context".into()); }
    }
    writeln!(out, "Patch: header EOL {}", transport.name()).map_err(|e| e.to_string())?;
    let mut budget = SEARCH_BUDGET;
    let mut clean = true;
    for section in &sections {
        let original = section.old.as_deref().map(|name| checked_path(&root, name)).transpose()?.flatten();
        if let Some(new) = section.new.as_deref()
            && section.old.as_deref() != Some(new) {
            let _ = checked_path(&root, new)?;
        }
        if !section.old_header || section.hunks.is_empty() {
            writeln!(out, "Section without hunks: metadata not analyzed").map_err(|e| e.to_string())?;
            clean = false;
            continue;
        }
        let name = section.old.as_deref().or(section.new.as_deref()).ok_or("file section has no path")?;
        writeln!(out, "\n=== {} ===", visible(name.as_bytes())).map_err(|e| e.to_string())?;
        if section.old.is_none() {
            writeln!(out, "Creation: no preimage to compare").map_err(|e| e.to_string())?;
            clean = false;
            continue;
        }
        let Some(path) = original else {
            writeln!(out, "FILE MISSING: no preimage to compare").map_err(|e| e.to_string())?;
            clean = false;
            continue;
        };
        let bytes = read_limited(&path, FILE_LIMIT)?;
        let line_count = bytes.iter().filter(|&&b| b == b'\n').take(SOURCE_LINES + 1).count()
            + usize::from(!bytes.is_empty() && !bytes.ends_with(b"\n"));
        if line_count > SOURCE_LINES {
            return Err(format!("{}: too many lines", path.display()));
        }
        let lines = source_lines(&bytes);
        let lf = lines.iter().filter(|l| l.eol == Eol::Lf).count();
        let crlf = lines.iter().filter(|l| l.eol == Eol::CrLf).count();
        let no_eol = lines.last().is_some_and(|l| l.eol == Eol::None);
        let lone_cr = lines.iter().map(|l| l.text.iter().filter(|&&b| b == b'\r').count()).sum::<usize>();
        writeln!(out, "File: LF={lf}, CRLF={crlf}, embedded CR={lone_cr}; no final LF: {}", if no_eol { "yes" } else { "no" })
            .map_err(|e| e.to_string())?;
        if section.new.is_none() && (section.hunks.len() != 1 || section.hunks[0].start != 1 || section.hunks[0].count != lines.len()) {
            writeln!(out, "Deletion: hunks do not cover the entire file").map_err(|e| e.to_string())?;
            clean = false;
        }
        for hunk in &section.hunks {
            let located = locate(hunk, &lines, &mut budget);
            clean &= matches!(located, Location::Hit { at, count: 1, quality: Quality::Strict } if at == hunk.expected());
            show(out, hunk, &lines, located, c.context, colors).map_err(|e| e.to_string())?;
        }
    }
    Ok(clean)
}
fn main() -> ExitCode {
    match options() {
        Ok(None) => { println!("Usage: diff-probe PATCH [--root DIR] [--context N] [--color auto|always|never]"); ExitCode::SUCCESS }
        Ok(Some(config)) => {
            let stdout = io::stdout();
            match execute(&config, &mut stdout.lock()) {
                Ok(true) => ExitCode::SUCCESS,
                Ok(false) => ExitCode::from(1),
                Err(e) => { eprintln!("diff-probe: {e}"); ExitCode::from(2) },
            }
        }
        Err(e) => { eprintln!("diff-probe: {e}"); ExitCode::from(2) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn only(patch: &[u8]) -> Hunk {
        let (mut sections, _) = parse(patch).unwrap();
        let mut file = sections.remove(0);
        file.hunks.remove(0)
    }
    fn find(hunk: &Hunk, source: &[u8]) -> Location {
        let mut budget = SEARCH_BUDGET;
        locate(hunk, &source_lines(source), &mut budget)
    }
    #[test]
    fn parser_reads_original_side_and_lf() {
        let h = only(b"--- a/x\n+++ b/x\n@@ -2 +2 @@\n-old\n+new\n");
        assert!(matches!(find(&h, b"before\nold\n"), Location::Hit { at: 1, quality: Quality::Strict, .. }));
    }
    #[test]
    fn moved_precedes_local_eol_mismatch() {
        let h = only(b"--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n");
        assert!(matches!(find(&h, b"old\r\nold\n"), Location::Hit { at: 1, quality: Quality::Strict, .. }));
    }
    #[test]
    fn eol_only_is_reported() {
        let h = only(b"--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n");
        assert!(matches!(find(&h, b"old\r\n"), Location::Hit { quality: Quality::Content, .. }));
    }
    #[test]
    fn unknown_eol_is_not_verified_as_exact() {
        let h = only(b"--- a/x\r\n+++ b/x\r\n@@ -1 +1 @@\r\n-old\r\n+new\r\n");
        assert!(matches!(find(&h, b"old\n"), Location::Hit { quality: Quality::Unknown, .. }));
        assert!(matches!(find(&h, b"old"), Location::Hit { quality: Quality::Content, .. }));
    }
    #[test]
    fn no_final_newline_marker_and_lone_cr() {
        let h = only(b"--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\r\n\\ No newline at end of file\n+new\n");
        assert_eq!(h.lines[0].text, b"old\r");
        assert_eq!(h.lines[0].eol, Eol::None);
        assert!(matches!(find(&h, b"old\r"), Location::Hit { quality: Quality::Strict, .. }));
    }
    #[test]
    fn mixed_structural_transport_is_rejected() {
        assert!(parse(b"--- a/x\n+++ b/x\n@@ -1 +1 @@\r\n-old\n+new\n").is_err());
    }
    #[test]
    fn hunk_lines_looking_like_headers_do_not_change_transport() {
        let h = only(b"--- a/x\n+++ b/x\n@@ -1 +1 @@\n--- comment\r\n+++ content\n");
        assert_eq!(h.lines[0].text, b"-- comment");
        assert_eq!(h.lines[0].eol, Eol::CrLf);
    }
    #[test]
    fn skips_git_format_patch_preamble_and_accepts_repeated_files() {
        let (sections, _) = parse(b"From abc\nSubject: [PATCH] edit\n---\ndiff --git a/x b/x\nindex abc..def\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\ndiff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -2 +2 @@\n-c\n+d\n-- \n2.0\n").unwrap();
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].hunks.len(), 1);
        assert_eq!(sections[1].hunks.len(), 1);
    }
    #[test]
    fn search_budget_must_report_undetermined() {
        let h = only(b"--- a/x\n+++ b/x\n@@ -1 +1 @@\n-no\n+yes\n");
        assert!(matches!(locate(&h, &source_lines(b"a\nb\n"), &mut 1), Location::Limited));
    }
    #[test]
    fn ambiguous_content_and_displaced_matches() {
        let h = only(b"--- a/x\n+++ b/x\n@@ -4 +4 @@\n-old\n+new\n");
        assert!(matches!(find(&h, b"old\nnew\nold\n"), Location::Hit { count: 2, .. }));
    }
    #[test]
    fn pure_insertion_shows_marker_even_with_zero_context() {
        let h = only(b"--- a/x\n+++ b/x\n@@ -1,0 +2 @@\n+new\n");
        let source = source_lines(b"a\nb\n");
        let mut out = Vec::new();
        show(&mut out, &h, &source, Location::Insertion(1), 0, false).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("insert after line 1"));
    }
    #[test]
    fn pure_insertion_marker_and_content_follow_previous_context() {
        let h = only(b"--- a/x\n+++ b/x\n@@ -1,0 +2 @@\n+new\n");
        let source = source_lines(b"before\nafter\n");
        let mut out = Vec::new();
        show(&mut out, &h, &source, Location::Insertion(1), 1, false).unwrap();
        let text = String::from_utf8(out).unwrap();
        let labels = ["  before\n", ">      insert after line 1\n", "+        new\n", "  after\n"];
        let mut from = 0;
        for label in labels {
            let at = text[from..].find(label).unwrap_or_else(|| panic!("missing or out of order: {label:?}\n{text}"));
            from += at + label.len();
        }
    }
    #[test]
    fn additions_appear_at_their_actual_hunk_positions() {
        let h = only(b"--- a/x\n+++ b/x\n@@ -1,4 +1,6 @@\n first\n+before-second\n second\n-third\n+replacement\n fourth\n+tail\n");
        let source = source_lines(b"first\nsecond\nthird\nfourth\noutside\n");
        let mut out = Vec::new();
        show(&mut out, &h, &source, Location::Hit { at: 0, count: 1, quality: Quality::Strict }, 1, false).unwrap();
        let text = String::from_utf8(out).unwrap();
        let labels = ["  first\n", "+        before-second\n", "  second\n", "  third\n",
                      "+        replacement\n", "  fourth\n", "+        tail\n", "  outside\n"];
        let mut from = 0;
        for label in labels {
            let at = text[from..].find(label).unwrap_or_else(|| panic!("missing or out of order: {label:?}\n{text}"));
            from += at + label.len();
        }
        assert_eq!(text.matches("+        ").count(), 3);
    }
    #[test]
    fn out_of_range_insertion_does_not_invent_context() {
        let h = only(b"--- a/x\n+++ b/x\n@@ -50,0 +51 @@\n+new\n");
        let source = source_lines(b"a\nb\n");
        let mut out = Vec::new();
        show(&mut out, &h, &source, Location::OutOfRange, 5, false).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("outside file"));
        assert!(text.contains("position 50"));
        assert!(!text.contains("     1  a"));
    }
    #[test]
    fn filename_quotes_and_escapes() {
        assert_eq!(filename(b"\"a/Zo\\303\\253.txt\"", b"a/").unwrap().as_deref(), Some("Zoë.txt"));
        assert_eq!(filename(b"b/file with spaces.txt", b"b/").unwrap().as_deref(), Some("file with spaces.txt"));
    }
    #[test]
    fn terminal_output_never_emits_untrusted_escape_sequences() {
        let text = visible(b"hello\x1b[31m\xe2\x80\xae");
        assert!(!text.contains('\u{1b}'));
        assert!(!text.contains('\u{202e}'));
        assert!(text.contains("\\u{1B}"));
    }
    #[test]
    fn extra_hunk_records_and_duplicate_eof_markers_are_rejected() {
        assert!(parse(b"--- a/x\n+++ b/x\n@@ -1 +1 @@\n-x\n+y\n-z\n").is_err());
        assert!(parse(b"--- a/x\n+++ b/x\n@@ -1 +1 @@\n-x\n+y\n\\ No newline at end of file\n\\ No newline at end of file\n").is_err());
    }
    #[test]
    fn deletion_cannot_add_and_hunks_cannot_overlap() {
        assert!(parse(b"--- a/x\n+++ /dev/null\n@@ -1 +1 @@\n-x\n+impossible\n").is_err());
        assert!(parse(b"--- a/x\n+++ b/x\n@@ -1 +1 @@\n-x\n+y\n@@ -1 +1 @@\n-x\n+z\n").is_err());
        assert!(parse(b"--- /dev/null\n+++ b/x\n@@ -1 +1 @@\n-x\n+y\n").is_err());
    }
    #[test]
    fn marker_cannot_appear_before_last_line_on_either_side() {
        assert!(parse(b"--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n first\n\\ No newline at end of file\n second\n").is_err());
        assert!(parse(b"--- a/x\n+++ b/x\n@@ -1 +1,2 @@\n-old\n+first\n\\ No newline at end of file\n+second\n").is_err());
    }
    #[cfg(windows)]
    #[test]
    fn windows_devices_and_ambiguous_suffixes_are_rejected() {
        for name in ["CONIN$", "CONOUT$", "CON .txt", "file.", "file "] {
            assert!(checked_path(Path::new("."), name).is_err(), "{name}");
        }
    }
    #[cfg(unix)]
    #[test]
    fn unix_names_are_not_subject_to_windows_restrictions() {
        let root = Path::new(".").canonicalize().unwrap();
        for name in ["CON", "CONIN$", "file.", "file ", "report:2026", "back\\slash"] {
            assert!(checked_path(&root, name).is_ok(), "{name}");
        }
        for name in ["/etc/passwd", "../escape", "a/../escape", "a//b", "a/./b"] {
            assert!(checked_path(&root, name).is_err(), "{name}");
        }
    }
    #[test]
    fn standalone_binary_diff_is_unverifiable() {
        let (sections, _) = parse(b"Binary files a/x and b/x differ\n").unwrap();
        assert_eq!(sections.len(), 1);
        assert!(sections[0].hunks.is_empty());
    }
    #[test]
    fn zero_length_hunks_may_share_an_anchor_without_consuming_old_lines() {
        let (sections, _) = parse(b"--- a/x\n+++ b/x\n@@ -5,0 +6,1 @@\n+first\n@@ -5,0 +7,1 @@\n+second\n").unwrap();
        assert_eq!(sections[0].hunks.len(), 2);
    }
    #[test]
    fn rejects_stray_records_before_the_first_hunk() {
        for record in [b"+stray\n".as_slice(), b"-stray\n", b"\\ No newline at end of file\n", b" stray\n"] {
            let mut diff = b"--- a/x\n+++ b/x\n".to_vec();
            diff.extend_from_slice(record);
            diff.extend_from_slice(b"@@ -1 +1 @@\n-old\n+new\n");
            assert!(parse(&diff).is_err(), "accepted: {record:?}");
        }
    }
    #[test]
    fn git_mail_preamble_does_not_interpret_commit_prose_as_diff_headers() {
        let mail = b"From abc Mon Sep 17 00:00:00 2001\nSubject: [PATCH] x\n\n--- note in the message\ndiff --git a/decoy b/decoy\n---\n x | 2 +-\n\ndiff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n";
        let (sections, _) = parse(mail).unwrap();
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].old.as_deref(), Some("x"));
        assert_eq!(sections[0].hunks.len(), 1);
    }
    #[test]
    fn git_mail_signature_is_not_parsed_as_hunk_records() {
        let mail = b"From abc Mon Sep 17 00:00:00 2001\nSubject: [PATCH] x\n---\ndiff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n-- \n - indented signature\n+signature\n--- signature text\nFrom def Mon Sep 17 00:00:00 2001\nSubject: [PATCH] y\n---\ndiff --git a/y b/y\n--- a/y\n+++ b/y\n@@ -1 +1 @@\n-old2\n+new2\n";
        let (sections, _) = parse(mail).unwrap();
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].hunks.len(), 1);
        assert_eq!(sections[1].hunks.len(), 1);
    }
    #[test]
    fn out_of_range_hunks_show_additions_after_missing_source_lines() {
        let h = only(b"--- a/x\n+++ b/x\n@@ -2,3 +2,4 @@\n first\n+inside\n-second\n+between\n-third\n+tail\n");
        let source = source_lines(b"prefix\nfirst\n");
        assert!(matches!(find(&h, b"prefix\nfirst\n"), Location::OutOfRange));
        let mut out = Vec::new();
        show(&mut out, &h, &source, Location::OutOfRange, 0, false).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("EOF before the hunk ends"));
        for label in ["+        inside", "+        between", "+        tail"] {
            assert!(text.contains(label), "missing {label}: {text}");
        }
        assert_eq!(text.matches("+        ").count(), 3);
    }
    #[test]
    fn ambiguous_matches_preserve_the_eol_quality_in_status() {
        let h = only(b"--- a/x\n+++ b/x\n@@ -4 +4 @@\n-old\n+new\n");
        let actual = find(&h, b"old\r\nother\nold\r\n");
        assert!(matches!(actual, Location::Hit { count: 2, quality: Quality::Content, .. }));
        assert!(status(actual, h.expected()).contains("EOL differs"));
        let h = only(b"--- a/x\r\n+++ b/x\r\n@@ -4 +4 @@\r\n-old\r\n+new\r\n");
        let actual = find(&h, b"old\nother\nold\n");
        assert!(matches!(actual, Location::Hit { count: 2, quality: Quality::Unknown, .. }));
        assert!(status(actual, h.expected()).contains("original EOL unknown"));
    }
    #[test]
    fn malformed_hunks_rejected() {
        assert!(parse(b"--- a/x\n+++ b/x\n@@ -1,2 +1 @@\n-old\n+new\n").is_err());
        assert!(parse(b"--- a/x\n+++ b/x\n@@ -0,0 +0,0 @@\n").is_err());
    }
}
