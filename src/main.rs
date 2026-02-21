use std::collections::HashSet;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read as _, Write};
use std::net::{IpAddr, SocketAddr};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use indicatif::{ProgressBar, ProgressStyle};
use rusqlite::{Connection, params};
use rustyline::completion::{Completer, FilenameCompleter, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::{Hinter, HistoryHinter};
use rustyline::history::DefaultHistory;
use rustyline::validate::Validator;
use rustyline::{
    CompletionType, Config, Context, Editor, ExternalPrinter, Helper,
};
use tesseras_dht::prelude::*;
use tokio::sync::mpsc;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::time::FormatTime;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Timeout for DHT network operations (store/retrieve).
const DHT_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum content size for inline store operations (1 MiB).
const MAX_CONTENT_SIZE: usize = 1024 * 1024;

/// Maximum file size for `put --file` operations (64 MiB).
const MAX_FILE_SIZE: usize = 64 * 1024 * 1024;

/// Adaptive timeout based on data size: 30s base + 2s per 256 KiB block, max 600s.
fn store_timeout(data_len: usize) -> Duration {
    let num_blocks = data_len.div_ceil(DEFAULT_BLOCK_SIZE);
    let secs = 30 + num_blocks as u64 * 2;
    Duration::from_secs(secs.min(600))
}

/// Adaptive timeout for retrieval based on chunk count: 30s base + 1s per chunk, max 600s.
fn retrieve_timeout(num_chunks: usize) -> Duration {
    let secs = 30 + num_chunks as u64;
    Duration::from_secs(secs.min(600))
}

const BANNER_LINES: &[&str] = &[
    r"████████╗███████╗███████╗███████╗███████╗██████╗  █████╗ ███████╗",
    r"╚══██╔══╝██╔════╝██╔════╝██╔════╝██╔════╝██╔══██╗██╔══██╗██╔════╝",
    r"   ██║   █████╗  ███████╗███████╗█████╗  ██████╔╝███████║███████╗",
    r"   ██║   ██╔══╝  ╚════██║╚════██║██╔══╝  ██╔══██╗██╔══██║╚════██║",
    r"   ██║   ███████╗███████║███████║███████╗██║  ██║██║  ██║███████║",
    r"   ╚═╝   ╚══════╝╚══════╝╚══════╝╚══════╝╚═╝  ╚═╝╚═╝  ╚═╝╚══════╝",
];

/// Query the terminal's background color via OSC 11.
///
/// Sends `\x1b]11;?\x1b\\` to `/dev/tty` and parses the
/// `rgb:RRRR/GGGG/BBBB` response. Returns `Some(true)` for light
/// backgrounds, `Some(false)` for dark, `None` if the terminal
/// doesn't respond (not a real tty, no OSC 11 support, etc.).
fn query_terminal_background() -> Option<bool> {
    let mut tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok()?;
    let fd = tty.as_raw_fd();

    // Save terminal state
    let old_termios = unsafe {
        let mut t = std::mem::zeroed::<libc::termios>();
        if libc::tcgetattr(fd, &mut t) != 0 {
            return None;
        }
        t
    };

    // Set raw mode: no echo, no canonical, 200ms read timeout
    let mut raw = old_termios;
    raw.c_lflag &= !(libc::ECHO | libc::ICANON);
    raw.c_cc[libc::VMIN] = 0;
    raw.c_cc[libc::VTIME] = 2; // 200ms
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
        return None;
    }

    // Send OSC 11 query
    let wrote = tty.write_all(b"\x1b]11;?\x1b\\").and_then(|_| tty.flush());

    let result = if wrote.is_ok() {
        // Read response (e.g. "\x1b]11;rgb:ffff/ffff/ffff\x1b\\")
        let mut buf = [0u8; 64];
        let mut total = 0;
        loop {
            match tty.read(&mut buf[total..]) {
                Ok(0) => break,
                Ok(n) => {
                    total += n;
                    // Check for ST (\x1b\\) or BEL (\x07) terminator
                    if buf[..total].contains(&0x07)
                        || buf[..total].windows(2).any(|w| w == b"\x1b\\")
                    {
                        break;
                    }
                    if total >= buf.len() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        std::str::from_utf8(&buf[..total])
            .ok()
            .and_then(parse_osc11_response)
    } else {
        None
    };

    // Restore terminal state
    unsafe {
        libc::tcsetattr(fd, libc::TCSANOW, &old_termios);
    }

    result
}

/// Parse an OSC 11 response to determine if the background is light.
///
/// Response format: `\x1b]11;rgb:RRRR/GGGG/BBBB\x1b\\`
/// Values are hex, either 2 or 4 digits per channel.
fn parse_osc11_response(response: &str) -> Option<bool> {
    let rgb_start = response.find("rgb:")?;
    let rgb_part = &response[rgb_start + 4..];
    let mut parts = rgb_part.splitn(3, '/');

    let r_str = parts.next()?;
    let g_str = parts.next()?;
    let b_str = parts.next()?;

    // Trim non-hex trailing chars (terminators)
    let hex_end =
        |s: &str| s.find(|c: char| !c.is_ascii_hexdigit()).unwrap_or(s.len());

    let r = u16::from_str_radix(&r_str[..hex_end(r_str)], 16).ok()?;
    let g = u16::from_str_radix(&g_str[..hex_end(g_str)], 16).ok()?;
    let b = u16::from_str_radix(&b_str[..hex_end(b_str)], 16).ok()?;

    // Normalize to 0–255
    let (r, g, b) = if r > 255 || g > 255 || b > 255 {
        ((r >> 8) as u8, (g >> 8) as u8, (b >> 8) as u8)
    } else {
        (r as u8, g as u8, b as u8)
    };

    // Perceived luminance (ITU-R BT.601)
    let luma = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
    Some(luma > 128.0)
}

/// Detect if the terminal has a light background.
///
/// Tries OSC 11 query first (works on most modern terminals),
/// falls back to COLORFG env var, then assumes dark.
fn is_light_terminal() -> bool {
    if let Some(light) = query_terminal_background() {
        return light;
    }
    // Fallback: COLORFG env var (iTerm2, some others)
    if let Ok(fg) = env::var("COLORFG") {
        let parts: Vec<u8> =
            fg.split(';').filter_map(|s| s.parse().ok()).collect();
        if parts.len() >= 3 {
            let luminance = parts[0] as u16 + parts[1] as u16 + parts[2] as u16;
            return luminance < 384;
        }
    }
    false
}

/// Print the TESSERAS banner with a grayscale gradient.
///
/// Uses true-color (24-bit) ANSI escapes for a smooth gradient.
/// Dark terminal: bright white fading to medium gray.
/// Light terminal: near-black fading to medium gray.
/// Respects NO_COLOR env var.
fn print_banner() {
    let no_color = env::var("NO_COLOR").is_ok();
    let light = is_light_terminal();
    let last = BANNER_LINES.len().saturating_sub(1).max(1);

    for (i, line) in BANNER_LINES.iter().enumerate() {
        if no_color {
            println!("{line}");
        } else {
            let t = i as f32 / last as f32;
            let gray = if light {
                // near-black (30) -> medium gray (140)
                (30.0 + 110.0 * t) as u8
            } else {
                // bright white (240) -> medium gray (120)
                (240.0 - 120.0 * t) as u8
            };
            println!("\x1b[38;2;{gray};{gray};{gray}m{line}\x1b[0m");
        }
    }

    // Version line — dimmed
    if no_color {
        println!("{:>66}", format!("v{VERSION}"));
    } else {
        let dim: u8 = if light { 100 } else { 140 };
        println!(
            "\x1b[38;2;{dim};{dim};{dim}m{:>66}\x1b[0m",
            format!("v{VERSION}")
        );
    }

    // Motto with current date/time — centered
    let now = format_utc_now();
    let motto = format!("Shut Up and Hack - {now}");
    if no_color {
        println!("{motto:^66}");
    } else {
        let dim: u8 = if light { 80 } else { 160 };
        println!("\x1b[1;3;38;2;{dim};{dim};{dim}m{motto:^66}\x1b[0m");
    }
    println!();
}

/// Format the current UTC time as `YYYY-MM-DD HH:MM`.
fn format_utc_now() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs();
    let days = secs / 86400;
    let day_secs = secs % 86400;
    let h = day_secs / 3600;
    let m = (day_secs % 3600) / 60;
    let z = days as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}")
}

/// Default listen port for the REPL node.
const DEFAULT_PORT: u16 = 4433;

struct CliOpts {
    bind_addr: Option<SocketAddr>,
    port: u16,
    ipv4_only: bool,
    data_dir: Option<PathBuf>,
}

/// Parse `-a ip[@port]` into a SocketAddr.
/// Default port is 0 (OS-assigned).
fn parse_addr(s: &str) -> Result<SocketAddr> {
    if let Some((ip_str, port_str)) = s.rsplit_once('@') {
        let ip: IpAddr = ip_str
            .parse()
            .map_err(|e| anyhow::anyhow!("bad IP '{ip_str}': {e}"))?;
        let port: u16 = port_str
            .parse()
            .map_err(|e| anyhow::anyhow!("bad port '{port_str}': {e}"))?;
        Ok(SocketAddr::new(ip, port))
    } else {
        let ip: IpAddr = s
            .parse()
            .map_err(|e| anyhow::anyhow!("bad IP '{s}': {e}"))?;
        Ok(SocketAddr::new(ip, 0))
    }
}

fn parse_args() -> Result<CliOpts> {
    let args: Vec<String> = env::args().collect();

    let mut opts = getopts::Options::new();
    opts.optflag("4", "", "Only listen to IPv4 connections");
    opts.optflag("6", "", "Only listen to IPv6 connections");
    opts.optflag("h", "help", "Print help information and exit");
    opts.optflag("v", "version", "Print version and exit");
    opts.optopt("a", "address", "Listen address ip[@port]", "ADDR");
    opts.optopt("D", "datadir", "Data directory", "DIR");
    opts.optopt(
        "p",
        "port",
        &format!("Listen port (default: {DEFAULT_PORT})"),
        "PORT",
    );

    let matches = opts.parse(&args[1..]).map_err(|e| anyhow::anyhow!("{e}"))?;

    if matches.opt_present("h") {
        eprint!("{}", opts.usage("Usage: tesseras-repl [options]"));
        process::exit(0);
    }

    if matches.opt_present("v") {
        eprintln!("tesseras-repl v{VERSION}");
        process::exit(0);
    }

    let ipv4_only = matches.opt_present("4");
    let ipv6_only = matches.opt_present("6");

    if ipv4_only && ipv6_only {
        anyhow::bail!("cannot use both -4 and -6");
    }

    let bind_addr = matches.opt_str("a").map(|s| parse_addr(&s)).transpose()?;

    let port = matches
        .opt_str("p")
        .map(|s| {
            s.parse::<u16>()
                .map_err(|e| anyhow::anyhow!("bad port '{s}': {e}"))
        })
        .transpose()?
        .unwrap_or(DEFAULT_PORT);

    if bind_addr.is_some() && matches.opt_present("p") {
        anyhow::bail!("-a and -p are mutually exclusive (use -a ip@port)");
    }

    let data_dir = matches.opt_str("D").map(PathBuf::from);

    Ok(CliOpts {
        bind_addr,
        port,
        ipv4_only,
        data_dir,
    })
}

/// Return the tesseras data directory, creating it if needed.
///
/// Uses `dirs::data_dir()` which resolves to the platform-appropriate
/// location: `~/.local/share` on Linux, `~/Library/Application Support`
/// on macOS, `{FOLDERID_RoamingAppData}` on Windows.
fn data_dir() -> Result<PathBuf> {
    let base = dirs::data_dir()
        .ok_or_else(|| anyhow::anyhow!("could not determine data directory"))?;
    let dir = base.join("tesseras");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

// -- Token database ----------------------------------------------------------

#[derive(Clone)]
struct TokenEntry {
    token: String,
    alias: Option<String>,
    created_at: String,
}

fn init_db(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
        CREATE TABLE IF NOT EXISTS tokens (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            token TEXT NOT NULL UNIQUE,
            alias TEXT UNIQUE,
            created_at TEXT NOT NULL
                DEFAULT (strftime('%Y-%m-%d %H:%M:%S', 'now'))
        );",
    )?;
    Ok(conn)
}

/// Read tokens from the legacy `hash.txt` file (one per line,
/// deduplicated, blank lines skipped).
fn load_tokens_from_file(path: &Path) -> Vec<String> {
    let Ok(file) = fs::File::open(path) else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.trim().is_empty())
        .filter(|l| seen.insert(l.clone()))
        .collect()
}

/// Migrate tokens from `hash.txt` into the SQLite database, then
/// rename the file to `hash.txt.bak`.
fn migrate_from_hash_txt(conn: &Connection, hash_path: &Path) -> Result<()> {
    let tokens = load_tokens_from_file(hash_path);
    if tokens.is_empty() {
        return Ok(());
    }
    for token in &tokens {
        conn.execute(
            "INSERT OR IGNORE INTO tokens (token) VALUES (?1)",
            params![token],
        )?;
    }
    let backup = hash_path.with_extension("txt.bak");
    fs::rename(hash_path, backup)?;
    Ok(())
}

fn load_entries(conn: &Connection) -> Result<Vec<TokenEntry>> {
    let mut stmt = conn
        .prepare("SELECT token, alias, created_at FROM tokens ORDER BY id")?;
    let entries = stmt
        .query_map([], |row| {
            Ok(TokenEntry {
                token: row.get(0)?,
                alias: row.get(1)?,
                created_at: row.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(entries)
}

/// Resolve a user argument to a `(index, &TokenEntry)` pair.
///
/// Resolution order:
/// 1. 1-based numeric index
/// 2. Exact alias match
/// 3. Exact token match
fn resolve_token<'a>(
    entries: &'a [TokenEntry],
    arg: &str,
) -> Option<(usize, &'a TokenEntry)> {
    if let Ok(idx) = arg.parse::<usize>()
        && idx >= 1
        && idx <= entries.len()
    {
        return Some((idx - 1, &entries[idx - 1]));
    }
    entries
        .iter()
        .enumerate()
        .find(|(_, e)| e.alias.as_deref() == Some(arg))
        .or_else(|| entries.iter().enumerate().find(|(_, e)| e.token == arg))
}

/// Parse `--name "alias"` or `--name alias` from the beginning of an
/// argument string.  Returns `(alias, remaining_content)`.
fn parse_name_flag(arg: &str) -> (Option<String>, &str) {
    let Some(rest) = arg.strip_prefix("--name ") else {
        return (None, arg);
    };
    let rest = rest.trim_start();
    if let Some(unquoted) = rest.strip_prefix('"')
        && let Some(end) = unquoted.find('"')
    {
        let alias = &unquoted[..end];
        let content = unquoted[end + 1..].trim_start();
        return (Some(alias.to_string()), content);
    }
    // Unquoted: alias runs to the next space
    if let Some((alias, content)) = rest.split_once(' ') {
        return (Some(alias.to_string()), content.trim_start());
    }
    // Only alias, no content (used by `edit --name foo`)
    (Some(rest.to_string()), "")
}

// -- Readline helper ---------------------------------------------------------

struct ReplHelper {
    file_completer: FilenameCompleter,
    hinter: HistoryHinter,
    tokens: Vec<String>,
    aliases: Vec<String>,
}

impl ReplHelper {
    fn new(entries: &[TokenEntry]) -> Self {
        Self {
            file_completer: FilenameCompleter::new(),
            hinter: HistoryHinter::new(),
            tokens: entries.iter().map(|e| e.token.clone()).collect(),
            aliases: entries.iter().filter_map(|e| e.alias.clone()).collect(),
        }
    }

    fn sync(&mut self, entries: &[TokenEntry]) {
        self.tokens = entries.iter().map(|e| e.token.clone()).collect();
        self.aliases = entries.iter().filter_map(|e| e.alias.clone()).collect();
    }
}

const COMMANDS: &[&str] = &[
    "put", "get", "delete", "alias", "info", "list", "edit", "help", "exit",
    "bye",
];

impl Completer for ReplHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        // After "get " or "delete ", complete from aliases then tokens
        let token_prefix = line
            .strip_prefix("get ")
            .or_else(|| line.strip_prefix("delete "));
        if let Some(prefix) = token_prefix {
            let prefix = prefix.trim_start();
            let start = line.len() - prefix.len();
            let mut matches: Vec<Pair> = Vec::new();

            // Aliases first
            let lower = prefix.to_lowercase();
            for alias in &self.aliases {
                if alias.to_lowercase().starts_with(&lower) {
                    matches.push(Pair {
                        display: alias.clone(),
                        replacement: alias.clone(),
                    });
                }
            }

            // Then tokens
            for t in &self.tokens {
                if t.starts_with(prefix) {
                    matches.push(Pair {
                        display: if t.len() > 20 {
                            format!("{}...", &t[..20])
                        } else {
                            t.clone()
                        },
                        replacement: t.clone(),
                    });
                }
            }

            if !matches.is_empty() {
                return Ok((start, matches));
            }
        }

        // Command completion at start of line
        if !line.contains(' ') {
            let matches: Vec<Pair> = COMMANDS
                .iter()
                .filter(|c| c.starts_with(line))
                .map(|c| Pair {
                    display: c.to_string(),
                    replacement: c.to_string(),
                })
                .collect();
            if !matches.is_empty() {
                return Ok((0, matches));
            }
        }

        // Fall back to filename completion
        self.file_completer.complete(line, pos, ctx)
    }
}

impl Hinter for ReplHelper {
    type Hint = String;

    fn hint(
        &self,
        line: &str,
        pos: usize,
        ctx: &Context<'_>,
    ) -> Option<String> {
        self.hinter.hint(line, pos, ctx)
    }
}

impl Highlighter for ReplHelper {}
impl Validator for ReplHelper {}
impl Helper for ReplHelper {}

// -- REPL state --------------------------------------------------------------

struct ReplState {
    node: NodeHandle,
    db: Connection,
    entries: Vec<TokenEntry>,
    token_set: HashSet<String>,
    tokens_changed: bool,
    public_key_hex: String,
}

impl ReplState {
    fn save_token(&mut self, token: String, alias: Option<&str>) {
        if self.token_set.contains(&token) {
            // Token exists — update alias if one was provided
            if let Some(alias) = alias {
                if let Err(e) = self.db.execute(
                    "UPDATE tokens SET alias = ?1 WHERE token = ?2",
                    params![alias, token],
                ) {
                    eprintln!("warning: failed to set alias: {e}");
                    return;
                }
                if let Some(entry) =
                    self.entries.iter_mut().find(|e| e.token == token)
                {
                    entry.alias = Some(alias.to_string());
                }
                self.tokens_changed = true;
            }
            return;
        }

        let result = match alias {
            Some(a) => self.db.execute(
                "INSERT INTO tokens (token, alias) VALUES (?1, ?2)",
                params![token, a],
            ),
            None => self.db.execute(
                "INSERT INTO tokens (token) VALUES (?1)",
                params![token],
            ),
        };

        match result {
            Ok(_) => {
                match self.db.query_row(
                    "SELECT created_at FROM tokens WHERE token = ?1",
                    params![token],
                    |row| row.get::<_, String>(0),
                ) {
                    Ok(created_at) => {
                        self.token_set.insert(token.clone());
                        self.entries.push(TokenEntry {
                            token,
                            alias: alias.map(String::from),
                            created_at,
                        });
                        self.tokens_changed = true;
                    }
                    Err(e) => {
                        eprintln!(
                            "warning: token saved but failed \
                             to read back: {e}"
                        );
                    }
                }
            }
            Err(e) => eprintln!("warning: failed to save token: {e}"),
        }
    }
}

fn open_editor() -> Result<String> {
    let editor = env::var("VISUAL")
        .or_else(|_| env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());

    let tmp = tempfile::Builder::new()
        .prefix("tesseras-")
        .suffix(".txt")
        .tempfile()?;

    let status = std::process::Command::new(&editor)
        .arg(tmp.path())
        .status()
        .map_err(|e| anyhow::anyhow!("failed to run editor '{editor}': {e}"))?;

    if !status.success() {
        anyhow::bail!("editor exited with {status}");
    }

    Ok(fs::read_to_string(tmp.path())?)
}

/// Timer that formats as `YYYY-MM-DD HH:MM:SS` (no fractional seconds).
struct CompactUtcTimer;

impl FormatTime for CompactUtcTimer {
    fn format_time(
        &self,
        w: &mut tracing_subscriber::fmt::format::Writer<'_>,
    ) -> std::fmt::Result {
        let d = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let secs = d.as_secs();
        // Days / time decomposition
        let days = secs / 86400;
        let day_secs = secs % 86400;
        let h = day_secs / 3600;
        let m = (day_secs % 3600) / 60;
        let s = day_secs % 60;
        // Date from days since 1970-01-01 (civil calendar algorithm)
        let z = days as i64 + 719468;
        let era = z.div_euclid(146097);
        let doe = z.rem_euclid(146097) as u64;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let y = yoe as i64 + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let mo = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if mo <= 2 { y + 1 } else { y };
        write!(w, "{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02}")
    }
}

/// Writer that routes output through rustyline's `ExternalPrinter`,
/// allowing log messages to appear without corrupting the prompt.
#[derive(Clone)]
struct ReplWriter {
    printer: Arc<Mutex<Box<dyn ExternalPrinter + Send>>>,
}

impl<'a> MakeWriter<'a> for ReplWriter {
    type Writer = ReplWriterGuard;

    fn make_writer(&'a self) -> Self::Writer {
        ReplWriterGuard(self.printer.clone())
    }
}

struct ReplWriterGuard(Arc<Mutex<Box<dyn ExternalPrinter + Send>>>);

impl std::io::Write for ReplWriterGuard {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let msg = String::from_utf8_lossy(buf).into_owned();
        self.0
            .lock()
            .unwrap()
            .print(msg)
            .map_err(std::io::Error::other)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// -- Command dispatch --------------------------------------------------------

/// Dispatch a REPL command. Returns false if the REPL should exit.
async fn handle_command(state: &mut ReplState, line: &str) -> Result<bool> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(true);
    }

    let (cmd, arg) = match line.split_once(' ') {
        Some((c, a)) => (c, a.trim()),
        None => (line, ""),
    };

    match cmd {
        "put" => {
            let (alias, content) = parse_name_flag(arg);

            // Check for --file flag in content
            let (data, is_file) = if let Some(file_path) =
                content.strip_prefix("--file ")
            {
                let file_path = file_path.trim();
                if file_path.is_empty() {
                    println!("usage: put [--name <alias>] --file <path>");
                    return Ok(true);
                }
                match tokio::fs::read(file_path).await {
                    Ok(data) => {
                        if data.len() > MAX_FILE_SIZE {
                            eprintln!(
                                "error: file too large ({} bytes, max {})",
                                data.len(),
                                MAX_FILE_SIZE
                            );
                            return Ok(true);
                        }
                        (data, true)
                    }
                    Err(e) => {
                        eprintln!("error: {e}");
                        return Ok(true);
                    }
                }
            } else {
                if content.is_empty() {
                    println!(
                        "usage: put [--name <alias>] [--file <path> | <content>]"
                    );
                    return Ok(true);
                }
                if content.len() > MAX_CONTENT_SIZE {
                    eprintln!(
                        "error: content too large ({} bytes, max {})",
                        content.len(),
                        MAX_CONTENT_SIZE
                    );
                    return Ok(true);
                }
                (content.as_bytes().to_vec(), false)
            };

            if let Some(ref a) = alias
                && state.entries.iter().any(|e| e.alias.as_deref() == Some(a))
            {
                eprintln!("error: alias '{a}' already exists");
                return Ok(true);
            }

            let timeout = store_timeout(data.len());
            let num_blocks = data.len().div_ceil(DEFAULT_BLOCK_SIZE).max(1);
            eprintln!(
                "storing {} bytes ({} block{}, timeout={}s)...",
                data.len(),
                num_blocks,
                if num_blocks == 1 { "" } else { "s" },
                timeout.as_secs()
            );
            let store_start = std::time::Instant::now();

            if is_file {
                // Use progress reporting for multi-block file stores
                let (progress_tx, mut progress_rx) = mpsc::channel(32);

                let pb = ProgressBar::new(num_blocks as u64);
                pb.set_style(
                    ProgressStyle::default_bar()
                        .template(
                            "{spinner:.green} [{bar:40}] {pos}/{len} blocks ({msg})",
                        )
                        .unwrap(),
                );

                // Scope the future so it drops before we access state mutably
                let result = {
                    let store_fut =
                        state.node.store_with_progress(&data, progress_tx);
                    tokio::pin!(store_fut);
                    loop {
                        tokio::select! {
                            Some(p) = progress_rx.recv() => {
                                match p {
                                    StoreProgress::Encoding { block, .. } =>
                                        pb.set_message(format!("encoding {}", block + 1)),
                                    StoreProgress::Distributing { block, .. } =>
                                        pb.set_message(format!("distributing {}", block + 1)),
                                    StoreProgress::BlockComplete { block, .. } =>
                                        pb.set_position(block as u64 + 1),
                                }
                            }
                            result = &mut store_fut => {
                                pb.finish_and_clear();
                                break result;
                            }
                        }
                    }
                };

                match result {
                    Ok(result) => {
                        eprintln!(
                            "store succeeded in {:.1}s",
                            store_start.elapsed().as_secs_f64()
                        );
                        let token = result.to_string();
                        println!("{token}");
                        state.save_token(token, alias.as_deref());
                    }
                    Err(e) => eprintln!(
                        "error: {e} (after {:.1}s)",
                        store_start.elapsed().as_secs_f64()
                    ),
                }
            } else {
                // Simple store (inline content or small files)
                match tokio::time::timeout(timeout, state.node.store(&data))
                    .await
                {
                    Ok(Ok(result)) => {
                        eprintln!(
                            "store succeeded in {:.1}s",
                            store_start.elapsed().as_secs_f64()
                        );
                        let token = result.to_string();
                        println!("{token}");
                        state.save_token(token, alias.as_deref());
                    }
                    Ok(Err(e)) => eprintln!(
                        "error: {e} (after {:.1}s)",
                        store_start.elapsed().as_secs_f64()
                    ),
                    Err(_) => eprintln!(
                        "error: store timed out (after {:.1}s)",
                        store_start.elapsed().as_secs_f64()
                    ),
                }
            }
        }

        "get" => {
            if arg.is_empty() {
                println!(
                    "usage: get [--file <token|index|alias> <output>] | <token|index|alias>"
                );
                return Ok(true);
            }

            // Check for --file flag
            if let Some(rest) = arg.strip_prefix("--file ") {
                let rest = rest.trim();
                let (token_arg, output_path) =
                    rest.rsplit_once(' ').ok_or_else(|| {
                        anyhow::anyhow!(
                            "usage: get --file <token|index|alias> <output>"
                        )
                    })?;
                let token_str = if let Some((_, entry)) =
                    resolve_token(&state.entries, token_arg)
                {
                    entry.token.clone()
                } else {
                    token_arg.to_string()
                };
                let parsed: StoreTesseraResult = token_str
                    .parse()
                    .map_err(|e| anyhow::anyhow!("invalid token: {e}"))?;
                let timeout = retrieve_timeout(parsed.chunk_hashes.len());
                let num_chunks = parsed.chunk_hashes.len();
                eprintln!(
                    "retrieving {} chunks (timeout={}s)...",
                    num_chunks,
                    timeout.as_secs()
                );

                let (progress_tx, mut progress_rx) = mpsc::channel(32);
                let pb = ProgressBar::new(num_chunks as u64);
                pb.set_style(
                    ProgressStyle::default_bar()
                        .template(
                            "{spinner:.green} [{bar:40}] {pos}/{len} chunks ({msg})",
                        )
                        .unwrap(),
                );

                let retrieve_result = {
                    let retrieve_fut =
                        state.node.retrieve_with_progress(&parsed, progress_tx);
                    let timed_fut = tokio::time::timeout(timeout, retrieve_fut);
                    tokio::pin!(timed_fut);
                    loop {
                        tokio::select! {
                            Some(p) = progress_rx.recv() => {
                                match p {
                                    RetrieveProgress::Fetching { chunk, .. } =>
                                        pb.set_message(format!("fetching {}", chunk + 1)),
                                    RetrieveProgress::ChunkComplete { chunk, .. } =>
                                        pb.set_position(chunk as u64 + 1),
                                }
                            }
                            result = &mut timed_fut => {
                                pb.finish_and_clear();
                                break result;
                            }
                        }
                    }
                };

                match retrieve_result {
                    Ok(Ok(data)) => {
                        tokio::fs::write(output_path, &data).await?;
                        eprintln!(
                            "wrote {} bytes to {}",
                            data.len(),
                            output_path
                        );
                        state.save_token(token_str, None);
                    }
                    Ok(Err(e)) => eprintln!("error: {e}"),
                    Err(_) => eprintln!("error: retrieve timed out"),
                }
            } else {
                let token_str = if let Some((_, entry)) =
                    resolve_token(&state.entries, arg)
                {
                    entry.token.clone()
                } else {
                    arg.to_string()
                };
                let parsed: StoreTesseraResult = token_str
                    .parse()
                    .map_err(|e| anyhow::anyhow!("invalid token: {e}"))?;
                let timeout = retrieve_timeout(parsed.chunk_hashes.len());
                match tokio::time::timeout(
                    timeout,
                    state.node.retrieve(&parsed),
                )
                .await
                {
                    Ok(Ok(data)) => {
                        let content = String::from_utf8_lossy(&data);
                        println!("{content}");
                        state.save_token(token_str, None);
                    }
                    Ok(Err(e)) => eprintln!("error: {e}"),
                    Err(_) => eprintln!("error: retrieve timed out"),
                }
            }
        }

        "delete" => {
            if arg.is_empty() {
                println!("usage: delete <token|index|alias>");
                return Ok(true);
            }
            if let Some((idx, entry)) = resolve_token(&state.entries, arg) {
                let token = entry.token.clone();
                state.db.execute(
                    "DELETE FROM tokens WHERE token = ?1",
                    params![token],
                )?;
                state.token_set.remove(&token);
                state.entries.remove(idx);
                state.tokens_changed = true;
                println!("deleted");
            } else {
                eprintln!("error: token not found: {arg}");
            }
        }

        "alias" => {
            if arg.is_empty() {
                println!("usage: alias <token|index> [name]");
                return Ok(true);
            }
            let (target, new_alias) = match arg.split_once(' ') {
                Some((t, a)) => {
                    let a = a.trim();
                    let a = a
                        .strip_prefix('"')
                        .and_then(|s| s.strip_suffix('"'))
                        .unwrap_or(a);
                    (t, if a.is_empty() { None } else { Some(a) })
                }
                None => (arg, None),
            };
            let Some((idx, _)) = resolve_token(&state.entries, target) else {
                eprintln!("error: token not found: {target}");
                return Ok(true);
            };
            if let Some(alias) = new_alias {
                if state
                    .entries
                    .iter()
                    .any(|e| e.alias.as_deref() == Some(alias))
                {
                    eprintln!("error: alias '{alias}' already exists");
                    return Ok(true);
                }
                let token = &state.entries[idx].token;
                state.db.execute(
                    "UPDATE tokens SET alias = ?1 WHERE token = ?2",
                    params![alias, token],
                )?;
                state.entries[idx].alias = Some(alias.to_string());
                state.tokens_changed = true;
                println!("alias set: {alias}");
            } else {
                let token = &state.entries[idx].token;
                state.db.execute(
                    "UPDATE tokens SET alias = NULL WHERE token = ?1",
                    params![token],
                )?;
                state.entries[idx].alias = None;
                state.tokens_changed = true;
                println!("alias removed");
            }
        }

        "info" => {
            let node_id = state.node.node_id();
            println!("  node id    : {node_id}");
            println!("  public key : {}", state.public_key_hex);
            println!("  listening  : {}", state.node.local_addr());

            let peer_count = state.node.routing_table_info().await.peer_count;
            println!("  peers      : {peer_count}");
            println!("  tokens     : {}", state.entries.len());
        }

        "list" => {
            if state.entries.is_empty() {
                println!("(no tokens stored)");
                return Ok(true);
            }

            let (limit, filter) = parse_list_args(arg);

            let filtered: Vec<(usize, &TokenEntry)> = state
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| match &filter {
                    Some(f) => e
                        .alias
                        .as_ref()
                        .is_some_and(|a| a.to_lowercase().contains(f)),
                    None => true,
                })
                .collect();

            let slice = if let Some(n) = limit {
                &filtered[filtered.len().saturating_sub(n)..]
            } else {
                &filtered
            };

            if slice.is_empty() {
                println!("(no matches)");
            } else {
                for (i, entry) in slice {
                    let ts = if entry.created_at.len() >= 16 {
                        &entry.created_at[..16]
                    } else {
                        &entry.created_at
                    };
                    let display = match &entry.alias {
                        Some(a) => a.clone(),
                        None => truncate_token(&entry.token, 40),
                    };
                    println!("  {}: [{ts}] {display}", i + 1);
                }
            }
        }

        "edit" => {
            let (alias, _) = parse_name_flag(arg);
            if let Some(ref a) = alias
                && state
                    .entries
                    .iter()
                    .any(|e| e.alias.as_deref() == Some(a.as_str()))
            {
                eprintln!("error: alias '{a}' already exists");
                return Ok(true);
            }
            match tokio::task::spawn_blocking(open_editor).await {
                Ok(Ok(content)) if content.trim().is_empty() => {
                    println!("(empty content, nothing stored)");
                }
                Ok(Ok(content)) if content.len() > MAX_CONTENT_SIZE => {
                    eprintln!(
                        "error: content too large ({} bytes, max {})",
                        content.len(),
                        MAX_CONTENT_SIZE
                    );
                }
                Ok(Ok(content)) => {
                    match tokio::time::timeout(
                        DHT_TIMEOUT,
                        state.node.store(content.as_bytes()),
                    )
                    .await
                    {
                        Ok(Ok(result)) => {
                            let token = result.to_string();
                            println!("{token}");
                            state.save_token(token, alias.as_deref());
                        }
                        Ok(Err(e)) => eprintln!("error: {e}"),
                        Err(_) => eprintln!("error: store timed out"),
                    }
                }
                Ok(Err(e)) => eprintln!("error: {e}"),
                Err(e) => eprintln!("error: {e}"),
            }
        }

        "help" | "?" => {
            println!("  put [--name <alias>] [--file <path> | <content>]");
            println!("               Store content or file in the DHT");
            println!(
                "  get [--file <token|index|alias> <output>] | <token|index|alias>"
            );
            println!(
                "               Retrieve content by token, index, or alias"
            );
            println!("  edit [--name <alias>]");
            println!("               Open $EDITOR to compose and store");
            println!("  delete <token|index|alias>");
            println!("               Remove a token from the local list");
            println!("  alias <token|index> [name]");
            println!("               Set or remove a token alias");
            println!("  list [filter] [-n N]");
            println!(
                "               List known tokens (filter by alias, -n last N)"
            );
            println!("  info         Show node information");
            println!("  help, ?      Show this help");
            println!("  exit         Quit the REPL");
        }

        "exit" | "bye" => return Ok(false),

        _ => println!(
            "unknown command: {cmd}. \
             try 'help' for a list of commands"
        ),
    }

    Ok(true)
}

fn truncate_token(token: &str, max: usize) -> String {
    let mut chars = token.chars();
    let truncated: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

/// Parse `list` arguments into an optional limit and optional filter.
///
/// Accepted forms:
/// - `list`                → (None, None)
/// - `list photo`          → (None, Some("photo"))
/// - `list -n 10`          → (Some(10), None)
/// - `list --last 10`      → (Some(10), None)
/// - `list photo -n 5`     → (Some(5), Some("photo"))
/// - `list -n 5 photo`     → (Some(5), Some("photo"))
fn parse_list_args(arg: &str) -> (Option<usize>, Option<String>) {
    let arg = arg.trim();
    if arg.is_empty() {
        return (None, None);
    }

    let parts: Vec<&str> = arg.split_whitespace().collect();
    let mut limit = None;
    let mut filter_parts = Vec::new();
    let mut i = 0;

    while i < parts.len() {
        if (parts[i] == "-n" || parts[i] == "--last")
            && i + 1 < parts.len()
            && let Ok(n) = parts[i + 1].parse::<usize>()
        {
            limit = Some(n);
            i += 2;
            continue;
        }
        filter_parts.push(parts[i]);
        i += 1;
    }

    let filter = if filter_parts.is_empty() {
        None
    } else {
        Some(filter_parts.join(" ").to_lowercase())
    };

    (limit, filter)
}

// -- Main --------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let opts = parse_args()?;

    // Data directories
    let base = match opts.data_dir {
        Some(ref d) => {
            fs::create_dir_all(d)?;
            d.clone()
        }
        None => data_dir()?,
    };
    let hashes_path = base.join("hash.txt");
    let db_path = base.join("tokens.db");
    let history_path = base.join("history.txt");
    let node_data_dir = base.join("node");

    // Initialize token database and migrate from hash.txt if needed
    let db = init_db(&db_path)?;
    if hashes_path.exists()
        && let Err(e) = migrate_from_hash_txt(&db, &hashes_path)
    {
        eprintln!("warning: failed to migrate hash.txt: {e}");
    }

    // Build node
    let mut builder = NodeBuilder::new(&node_data_dir)
        .bootstrap(BootstrapSource::Dns("tesseras.net".into()))
        .on_spawn_progress(|step| match step {
            SpawnProgress::LoadingIdentity => {
                eprintln!("Loading identity...");
            }
            SpawnProgress::GeneratingPow => {
                eprintln!("Generating proof-of-work (first run)...");
            }
            SpawnProgress::BindingTransport { addr } => {
                eprintln!("Binding to {addr}...");
            }
            SpawnProgress::SpawningNode => {
                eprintln!("Spawning node...");
            }
            SpawnProgress::ResolvingBootstrap { ref domain } => {
                eprintln!("Resolving bootstrap ({domain})...");
            }
            SpawnProgress::ConnectingBootstrap { count } => {
                eprintln!(
                    "Connecting to {count} bootstrap peer{}...",
                    if count == 1 { "" } else { "s" }
                );
            }
            SpawnProgress::StartingMdns => {
                eprintln!("Starting mDNS discovery...");
            }
        });

    // Apply bind address from CLI flags.
    // Default: [::]:4433 (dual-stack). -4: 0.0.0.0:port. -6: [::]:port.
    if let Some(addr) = opts.bind_addr {
        builder = builder.bind(addr);
    } else {
        let ip: IpAddr = if opts.ipv4_only {
            "0.0.0.0".parse().unwrap()
        } else {
            "::".parse().unwrap()
        };
        builder = builder.bind(SocketAddr::new(ip, opts.port));
    }

    let node = builder.spawn().await?;
    eprintln!(
        "Node spawned: node_id={} local_addr={}",
        node.node_id(),
        node.local_addr()
    );
    if let Some(ext) = node.external_addr().await {
        eprintln!("External address (from bootstrap): {}", ext);
    } else {
        eprintln!(
            "External address: not yet discovered (no bootstrap response?)"
        );
    }

    // Read public key from persisted identity
    let public_key_hex = {
        let metadata = MetadataStore::open(&node_data_dir.join("metadata.db"))?;
        match metadata.load_identity()? {
            Some((mut secret, _, _)) => {
                let kp = Keypair::from_secret_bytes(&secret);
                let hex = hex::encode(kp.public_key_bytes());
                // Zero the secret key bytes before dropping
                secret.fill(0);
                hex
            }
            None => "(unknown)".to_string(),
        }
    };

    eprintln!();
    print_banner();
    let no_color = env::var("NO_COLOR").is_ok();
    if no_color {
        eprintln!("Node {} listening on {}", node.node_id(), node.local_addr());
    } else {
        eprintln!(
            "Node \x1b[1m{}\x1b[0m listening on \x1b[1m{}\x1b[0m",
            node.node_id(),
            node.local_addr()
        );
    }
    eprintln!();

    // Load token entries from database
    let entries = load_entries(&db)?;
    let token_set: HashSet<String> =
        entries.iter().map(|e| e.token.clone()).collect();

    let mut state = ReplState {
        node,
        db,
        entries: entries.clone(),
        token_set,
        tokens_changed: false,
        public_key_hex,
    };

    // Set up rustyline
    let config = Config::builder()
        .completion_type(CompletionType::List)
        .build();
    let helper = ReplHelper::new(&entries);
    let mut rl: Editor<ReplHelper, DefaultHistory> =
        Editor::with_config(config)?;
    rl.set_helper(Some(helper));
    let _ = rl.load_history(&history_path);

    // Route tracing output through rustyline's ExternalPrinter so
    // log messages don't corrupt the prompt.
    let printer = rl.create_external_printer()?;
    let boxed: Box<dyn ExternalPrinter + Send> = Box::new(printer);
    let repl_writer = ReplWriter {
        printer: Arc::new(Mutex::new(boxed)),
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "error".parse().unwrap()),
        )
        .with_target(false)
        .with_timer(CompactUtcTimer)
        .compact()
        .with_writer(repl_writer)
        .with_ansi(false)
        .init();

    // REPL loop
    loop {
        match rl.readline("tesseras> ") {
            Ok(line) => {
                let _ = rl.add_history_entry(&line);

                let result = handle_command(&mut state, &line).await;

                // Sync entries to helper only when changed
                if state.tokens_changed {
                    if let Some(h) = rl.helper_mut() {
                        h.sync(&state.entries);
                    }
                    state.tokens_changed = false;
                }

                match result {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(e) => eprintln!("error: {e}"),
                }
            }
            Err(ReadlineError::Eof) => {
                // Ctrl-D
                println!("bye");
                break;
            }
            Err(ReadlineError::Interrupted) => {
                // Ctrl-C in readline — continue
                continue;
            }
            Err(e) => {
                eprintln!("readline error: {e}");
                break;
            }
        }
    }

    // Shutdown
    eprintln!("Shutting down node...");
    state.node.shutdown().await;
    let _ = rl.save_history(&history_path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc11_dark_background() {
        let resp = "\x1b]11;rgb:0000/0000/0000\x1b\\";
        assert_eq!(parse_osc11_response(resp), Some(false));
    }

    #[test]
    fn osc11_light_background() {
        let resp = "\x1b]11;rgb:ffff/ffff/ffff\x1b\\";
        assert_eq!(parse_osc11_response(resp), Some(true));
    }

    #[test]
    fn osc11_8bit_values() {
        let resp = "\x1b]11;rgb:ff/ff/ff\x1b\\";
        assert_eq!(parse_osc11_response(resp), Some(true));
    }

    #[test]
    fn osc11_mid_gray_is_dark() {
        // rgb 128,128,128 -> luma = 128.0, not > 128 -> dark
        let resp = "\x1b]11;rgb:8080/8080/8080\x1b\\";
        assert_eq!(parse_osc11_response(resp), Some(false));
    }

    #[test]
    fn osc11_invalid_input() {
        assert_eq!(parse_osc11_response("garbage"), None);
        assert_eq!(parse_osc11_response(""), None);
    }

    #[test]
    fn osc11_bel_terminator() {
        let resp = "\x1b]11;rgb:ffff/ffff/ffff\x07";
        assert_eq!(parse_osc11_response(resp), Some(true));
    }

    #[test]
    fn addr_ip_only() {
        let addr = parse_addr("127.0.0.1").unwrap();
        assert_eq!(addr.ip(), "127.0.0.1".parse::<IpAddr>().unwrap());
        assert_eq!(addr.port(), 0);
    }

    #[test]
    fn addr_with_port() {
        let addr = parse_addr("127.0.0.1@8080").unwrap();
        assert_eq!(addr.ip(), "127.0.0.1".parse::<IpAddr>().unwrap());
        assert_eq!(addr.port(), 8080);
    }

    #[test]
    fn addr_ipv6() {
        let addr = parse_addr("::1@9000").unwrap();
        assert_eq!(addr.ip(), "::1".parse::<IpAddr>().unwrap());
        assert_eq!(addr.port(), 9000);
    }

    #[test]
    fn addr_invalid_ip() {
        assert!(parse_addr("not-an-ip").is_err());
    }

    #[test]
    fn addr_invalid_port() {
        assert!(parse_addr("127.0.0.1@notaport").is_err());
    }

    #[test]
    fn tokens_deduplicates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tokens.txt");
        fs::write(&path, "abc\nabc\ndef\nabc\ndef\nghi\n").unwrap();
        let tokens = load_tokens_from_file(&path);
        assert_eq!(tokens, vec!["abc", "def", "ghi"]);
    }

    #[test]
    fn tokens_skips_blank_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tokens.txt");
        fs::write(&path, "abc\n\n  \ndef\n").unwrap();
        let tokens = load_tokens_from_file(&path);
        assert_eq!(tokens, vec!["abc", "def"]);
    }

    #[test]
    fn tokens_missing_file() {
        let tokens =
            load_tokens_from_file(Path::new("/nonexistent/tokens.txt"));
        assert!(tokens.is_empty());
    }

    // -- Database tests ------------------------------------------------------

    #[test]
    fn db_init_and_insert() {
        let dir = tempfile::tempdir().unwrap();
        let db = init_db(&dir.path().join("test.db")).unwrap();
        db.execute("INSERT INTO tokens (token) VALUES (?1)", params!["tok1"])
            .unwrap();
        let entries = load_entries(&db).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].token, "tok1");
        assert!(entries[0].alias.is_none());
        assert!(!entries[0].created_at.is_empty());
    }

    #[test]
    fn db_alias_unique() {
        let dir = tempfile::tempdir().unwrap();
        let db = init_db(&dir.path().join("test.db")).unwrap();
        db.execute(
            "INSERT INTO tokens (token, alias) VALUES (?1, ?2)",
            params!["tok1", "name1"],
        )
        .unwrap();
        let result = db.execute(
            "INSERT INTO tokens (token, alias) VALUES (?1, ?2)",
            params!["tok2", "name1"],
        );
        assert!(result.is_err());
    }

    #[test]
    fn db_migrate_from_hash_txt() {
        let dir = tempfile::tempdir().unwrap();
        let hash_path = dir.path().join("hash.txt");
        fs::write(&hash_path, "tok1\ntok2\ntok1\n").unwrap();

        let db = init_db(&dir.path().join("tokens.db")).unwrap();
        migrate_from_hash_txt(&db, &hash_path).unwrap();

        let entries = load_entries(&db).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].token, "tok1");
        assert_eq!(entries[1].token, "tok2");

        // hash.txt should be renamed
        assert!(!hash_path.exists());
        assert!(dir.path().join("hash.txt.bak").exists());
    }

    // -- Resolve tests -------------------------------------------------------

    fn sample_entries() -> Vec<TokenEntry> {
        vec![
            TokenEntry {
                token: "aaa".into(),
                alias: Some("photo".into()),
                created_at: "2026-01-01 00:00:00".into(),
            },
            TokenEntry {
                token: "bbb".into(),
                alias: None,
                created_at: "2026-01-02 00:00:00".into(),
            },
            TokenEntry {
                token: "ccc".into(),
                alias: Some("diary".into()),
                created_at: "2026-01-03 00:00:00".into(),
            },
        ]
    }

    #[test]
    fn resolve_by_index() {
        let entries = sample_entries();
        let (idx, e) = resolve_token(&entries, "2").unwrap();
        assert_eq!(idx, 1);
        assert_eq!(e.token, "bbb");
    }

    #[test]
    fn resolve_by_alias() {
        let entries = sample_entries();
        let (idx, e) = resolve_token(&entries, "diary").unwrap();
        assert_eq!(idx, 2);
        assert_eq!(e.token, "ccc");
    }

    #[test]
    fn resolve_by_token() {
        let entries = sample_entries();
        let (idx, e) = resolve_token(&entries, "aaa").unwrap();
        assert_eq!(idx, 0);
        assert_eq!(e.token, "aaa");
    }

    #[test]
    fn resolve_not_found() {
        let entries = sample_entries();
        assert!(resolve_token(&entries, "999").is_none());
        assert!(resolve_token(&entries, "nope").is_none());
    }

    #[test]
    fn resolve_index_zero_is_invalid() {
        let entries = sample_entries();
        assert!(resolve_token(&entries, "0").is_none());
    }

    // -- Name flag parsing ---------------------------------------------------

    #[test]
    fn name_flag_quoted() {
        let (alias, content) =
            parse_name_flag("--name \"my photo\" hello world");
        assert_eq!(alias.as_deref(), Some("my photo"));
        assert_eq!(content, "hello world");
    }

    #[test]
    fn name_flag_unquoted() {
        let (alias, content) = parse_name_flag("--name photo hello world");
        assert_eq!(alias.as_deref(), Some("photo"));
        assert_eq!(content, "hello world");
    }

    #[test]
    fn name_flag_only_alias() {
        let (alias, content) = parse_name_flag("--name diary");
        assert_eq!(alias.as_deref(), Some("diary"));
        assert_eq!(content, "");
    }

    #[test]
    fn name_flag_absent() {
        let (alias, content) = parse_name_flag("just some content");
        assert!(alias.is_none());
        assert_eq!(content, "just some content");
    }

    // -- List args parsing ---------------------------------------------------

    #[test]
    fn list_args_empty() {
        assert_eq!(parse_list_args(""), (None, None));
    }

    #[test]
    fn list_args_filter() {
        assert_eq!(parse_list_args("photo"), (None, Some("photo".into())));
    }

    #[test]
    fn list_args_last_n() {
        assert_eq!(parse_list_args("-n 5"), (Some(5), None));
        assert_eq!(parse_list_args("--last 10"), (Some(10), None));
    }

    #[test]
    fn list_args_filter_with_limit() {
        assert_eq!(
            parse_list_args("photo -n 5"),
            (Some(5), Some("photo".into()))
        );
        assert_eq!(
            parse_list_args("-n 5 photo"),
            (Some(5), Some("photo".into()))
        );
    }

    #[test]
    fn truncate_multibyte() {
        // Emoji: each is 4 bytes, should not panic
        let s = "abcde";
        assert_eq!(truncate_token(s, 3), "abc...");
    }

    // -- Truncate ------------------------------------------------------------

    #[test]
    fn truncate_short() {
        assert_eq!(truncate_token("abc", 10), "abc");
    }

    #[test]
    fn truncate_long() {
        let long = "a".repeat(50);
        let result = truncate_token(&long, 10);
        assert_eq!(result, format!("{}...", "a".repeat(10)));
    }
}
