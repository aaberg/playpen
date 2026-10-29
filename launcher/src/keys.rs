//! keys.conf handling: parsing, permission tightening, op:// resolution and
//! the throwaway 0600 env-file passed to the engine.

use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

use tempfile::NamedTempFile;

use crate::cli::Config;
use crate::host::Host;
use crate::signals;

/// A parsed NAME=value entry; the value is pre-resolution (op:// intact).
#[derive(Debug, PartialEq)]
pub struct Entry {
    pub name: Vec<u8>,
    pub value: Vec<u8>,
}

/// The temp env-file holding the resolved keys. Deleting on drop mirrors the
/// bash EXIT trap.
pub struct EnvFile {
    file: NamedTempFile,
}

impl EnvFile {
    pub fn path(&self) -> &Path {
        self.file.path()
    }
}

/// Load and resolve the keys file. `None` means: launch without keys.
/// Fatal problems print the bash launcher's exact messages and exit 1.
pub fn load(config: &Config, host: &Host) -> Option<EnvFile> {
    let keys_file = &config.keys_file;
    if !keys_file.is_file() {
        if config.keys_explicit {
            signals::fail(format!("playpen: keys file not found: {}", keys_file.display()));
        }
        eprintln!(
            "playpen: {} not found - launching without API keys",
            keys_file.display()
        );
        return None;
    }
    tighten_permissions(keys_file);
    let bytes = match fs::read(keys_file) {
        Ok(bytes) => bytes,
        Err(err) => {
            signals::fail(format!(
                "playpen: could not read keys file {}: {err}",
                keys_file.display()
            ));
        }
    };
    let entries = parse(&bytes);
    if entries.is_empty() {
        return None;
    }
    let mut file = match NamedTempFile::with_prefix_in("playpen-env.", &host.runtime_dir) {
        Ok(file) => file,
        Err(err) => {
            signals::fail(format!(
                "playpen: could not create temp file in {}: {err}",
                host.runtime_dir.display()
            ));
        }
    };
    // from here on, every exit path goes through signals::fail, which
    // deletes this file - the bash trap's job
    signals::register_env_file(file.path().to_path_buf());

    let mut body = Vec::new();
    for entry in &entries {
        let value = if entry.value.starts_with(b"op://") {
            match resolve_op(&entry.value) {
                Some(value) => value,
                None => signals::fail(format!(
                    "playpen: could not resolve 1Password reference for {}",
                    String::from_utf8_lossy(&entry.name)
                )),
            }
        } else {
            entry.value.clone()
        };
        body.extend_from_slice(&entry.name);
        body.push(b'=');
        body.extend_from_slice(&value);
        body.push(b'\n');
    }
    if let Err(err) = file.as_file_mut().write_all(&body) {
        signals::fail(format!("playpen: could not write keys env file: {err}"));
    }
    let _ = file
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o600));
    Some(EnvFile { file })
}

/// Parse keys.conf the way the bash loop did: `IFS='=' read -r name value`
/// splits at the *first* '=' (values keep embedded '='), names lose all
/// whitespace, '#' comments and blank names/values are skipped, and one
/// trailing '\r' is tolerated (CRLF files). A line without '=' leaves the
/// value empty and is therefore skipped.
pub fn parse(bytes: &[u8]) -> Vec<Entry> {
    let mut entries = Vec::new();
    for line in bytes.split_inclusive(|b| *b == b'\n') {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        let (name, value): (&[u8], &[u8]) = match line.iter().position(|b| *b == b'=') {
            Some(eq) => (&line[..eq], &line[eq + 1..]),
            None => (line, b""),
        };
        let name = strip_space(name);
        if name.is_empty() || name[0] == b'#' {
            continue;
        }
        let value = value.strip_suffix(b"\r").unwrap_or(value);
        if value.is_empty() {
            continue;
        }
        entries.push(Entry {
            name,
            value: value.to_vec(),
        });
    }
    entries
}

/// bash `${name//[[:space:]]/}`: space, \t, \n, \v, \f, \r.
fn strip_space(bytes: &[u8]) -> Vec<u8> {
    bytes
        .iter()
        .copied()
        .filter(|b| !matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))
        .collect()
}

/// `value="$(op read "$value")"`: stdout captured, stderr inherited (the CLI
/// shows its own prompts and errors), all trailing newlines stripped like
/// command substitution. `None` means the reference did not resolve.
fn resolve_op(reference: &[u8]) -> Option<Vec<u8>> {
    let output = Command::new("op")
        .arg("read")
        .arg(OsStr::from_bytes(reference))
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output();
    match output {
        Ok(out) if out.status.success() => {
            let mut value = out.stdout;
            while value.last() == Some(&b'\n') {
                value.pop();
            }
            Some(value)
        }
        Ok(_) => None, // op reported its own error on stderr
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("playpen: op: command not found");
            None
        }
        Err(err) => {
            eprintln!("playpen: could not run op: {err}");
            None
        }
    }
}

/// `stat -c %a` + chmod 600 when group/other bits are set.
fn tighten_permissions(keys_file: &Path) {
    if let Ok(meta) = fs::metadata(keys_file) {
        if meta.permissions().mode() & 0o077 != 0 {
            if let Err(err) =
                fs::set_permissions(keys_file, fs::Permissions::from_mode(0o600))
            {
                signals::fail(format!(
                    "playpen: could not tighten {}: {err}",
                    keys_file.display()
                ));
            }
            eprintln!(
                "playpen: {} was group/other-readable - tightened to 600",
                keys_file.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(s: &str) -> Vec<Entry> {
        parse(s.as_bytes())
    }

    fn entry(name: &str, value: &str) -> Entry {
        Entry {
            name: name.as_bytes().to_vec(),
            value: value.as_bytes().to_vec(),
        }
    }

    #[test]
    fn parses_name_value_pairs() {
        assert_eq!(
            parse_str("MISTRAL_API_KEY=abc123\n"),
            vec![entry("MISTRAL_API_KEY", "abc123")]
        );
    }

    #[test]
    fn skips_comments_blanks_and_valueless_lines() {
        let input = "# comment\n\
                     \n\
                     GARBAGE_WITHOUT_EQUALS\n\
                     EMPTY=\n\
                     =LEADING_EQ\n\
                     MISTRAL_API_KEY=abc\n";
        assert_eq!(parse_str(input), vec![entry("MISTRAL_API_KEY", "abc")]);
    }

    #[test]
    fn strips_all_whitespace_from_names_only() {
        let input = " MY KEY \t= value \n";
        assert_eq!(parse_str(input), vec![entry("MYKEY", " value ")]);
    }

    #[test]
    fn comment_check_happens_after_whitespace_strip() {
        assert_eq!(parse_str("   # indented comment\n"), vec![]);
    }

    #[test]
    fn crlf_tolerated_by_stripping_one_trailing_cr() {
        assert_eq!(parse_str("KEY=value\r\n"), vec![entry("KEY", "value")]);
        // only one \r is stripped, exactly like bash `${value%$'\r'}`
        assert_eq!(parse_str("KEY=value\r\r\n"), vec![entry("KEY", "value\r")]);
        // the last line needs no trailing newline
        assert_eq!(parse_str("KEY=value"), vec![entry("KEY", "value")]);
    }

    #[test]
    fn values_keep_embedded_equals() {
        assert_eq!(parse_str("KEY=a=b=c\n"), vec![entry("KEY", "a=b=c")]);
    }

    #[test]
    fn op_references_are_kept_verbatim_for_resolution() {
        assert_eq!(
            parse_str("MISTRAL_API_KEY=op://Private/Mistral API Key/credential\n"),
            vec![entry("MISTRAL_API_KEY", "op://Private/Mistral API Key/credential")]
        );
    }

    #[test]
    fn empty_file_yields_no_entries() {
        assert_eq!(parse_str(""), vec![]);
        assert_eq!(parse_str("\n\n# only comments\n"), vec![]);
    }
}
